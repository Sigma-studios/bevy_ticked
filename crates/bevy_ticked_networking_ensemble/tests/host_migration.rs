//! A host change, through the bridge.
//!
//! The snapshot model cannot carry a match across a host change — the authoritative world was the
//! old host's — so the bridge ends the ticked session on every survivor and starts a fresh one with
//! the new host in the same lobby. These drive the lobby crate's migration over the loopback
//! harness and check the three things that can go wrong in between: a role never taken back, a
//! role taken back too early, and state from the old session leaking into the new one.

use std::time::Duration;

use bevy::prelude::*;
use bevy_ticked::tracked_entity::TickTrackedEntity;
use bevy_ticked_networking_ensemble::{HandshakeTimedOut, LocalSpawnerSlot, SpawnerSlots};
use bevy_ticked_testing::fixtures::minimal::{self, Input, seat_everyone};
use bevy_ticked_testing::prelude::*;

const SETTLE: usize = 400;

const MIGRATION: HostMigratable = HostMigratable {
    successor_within: Duration::from_secs(4),
    reach_within: Duration::from_secs(20),
};

fn frames(duration: Duration) -> usize {
    (duration.as_secs_f64() / TICK.as_secs_f64()).ceil() as usize
}

fn tracked(net: &mut TickedNetwork, peer: PeerId) -> usize {
    let world = net.world_mut(peer);
    world
        .query_filtered::<(), With<TickTrackedEntity>>()
        .iter(world)
        .count()
}

/// A settled session of a host and two clients, every player seated, in a lobby that migrates.
fn seated_session() -> (TickedNetwork, PeerId, PeerId) {
    let mut net = TickedNetwork::client_server::<Input>(2, minimal::install)
        .with_link(Link::cable())
        .with_seed(11)
        .with_host_migration(MIGRATION);
    assert!(net.settle(SETTLE), "the session did not settle");
    seat_everyone(&mut net);
    net.run(30);
    let clients = net.clients();
    (net, clients[0], clients[1])
}

#[test]
fn a_host_change_ends_the_snapshot_session_for_every_survivor() {
    let (mut net, a, b) = seated_session();
    assert!(tracked(&mut net, b) > 0, "the old session had a world");

    net.migrate(a);
    net.step();
    for peer in [a, b] {
        assert_eq!(role(net.app(peer)), Role::Solo, "no role on peer {peer:?}");
        assert_eq!(
            tracked(&mut net, peer),
            0,
            "the old session's world is gone on peer {peer:?}"
        );
    }
}

#[test]
fn the_new_host_and_the_client_that_stays_start_a_fresh_session() {
    let (mut net, a, b) = seated_session();
    net.migrate(a);

    let started = net.run_until(SETTLE, |net| {
        role(net.app(a)) == Role::Host
            && role(net.app(b)) == Role::Client
            && applied_tick(net.app(b)).is_some()
    });
    assert!(
        started,
        "a hosts and b applies its snapshots: a is {:?}, b is {:?}, b applied {:?}",
        role(net.app(a)),
        role(net.app(b)),
        applied_tick(net.app(b))
    );
    assert!(net.settle(SETTLE), "and the new session settles");
}

#[test]
fn the_new_host_hands_out_spawner_slots_from_one() {
    let (mut net, a, b) = seated_session();
    net.migrate(a);
    assert!(net.settle(SETTLE));
    let b_uuid = net.uuid(b);

    assert_eq!(
        net.app(a)
            .world()
            .resource::<SpawnerSlots>()
            .iter()
            .collect::<Vec<_>>(),
        [(b_uuid, 1)],
        "only b holds a slot, and the old session's assignments are gone"
    );
    assert_eq!(
        net.app(b)
            .world()
            .get_resource::<LocalSpawnerSlot>()
            .map(|slot| slot.0.0),
        Some(1)
    );
}

#[test]
fn a_slow_reconnect_to_the_new_host_does_not_time_out_the_registry_handshake() {
    let (mut net, a, b) = seated_session();
    net.lose_host(HostDeparture::Crashes);
    net.disconnect(b);
    net.name_host(a);

    // Longer than the registry handshake's own timeout, with b unable to reach its new host.
    net.run(frames(Duration::from_secs(6)));
    net.reconnect(b);

    let joined = net.run_until(SETTLE, |net| {
        role(net.app(b)) == Role::Client && applied_tick(net.app(b)).is_some()
    });
    assert!(
        joined,
        "b joined the new session once it could reach the host"
    );
    assert!(
        !net.app(b).world().contains_resource::<HandshakeTimedOut>(),
        "and the wait for the connection was not counted against the handshake"
    );
}

#[test]
fn a_session_that_is_never_given_a_new_host_ends_with_its_lobby() {
    let mut net = TickedNetwork::client_server::<Input>(1, minimal::install)
        .with_link(Link::cable())
        .with_host_migration(HostMigratable {
            successor_within: Duration::from_millis(300),
            ..MIGRATION
        });
    assert!(net.settle(SETTLE));
    let client = net.client();
    net.lose_host(HostDeparture::Crashes);
    net.run(frames(Duration::from_millis(600)));
    assert_eq!(role(net.app(client)), Role::Solo);
    assert_eq!(tracked(&mut net, client), 0);
}
