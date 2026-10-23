//! A client whose link to its host breaks, and is made again, over the harness.
//!
//! `break_link` is the connection failing at both ends while the arbiter still has both in the
//! lobby; `reconnect` is the host reaching the client again. The lobby layer holds the seat and
//! the client waits; what this crate owes it is a clock that stands still meanwhile, a session
//! that carries on afterwards with the same bodies, and nothing from the old connection read as
//! if it belonged to the new one.

use std::time::Duration;

use bevy::prelude::*;
use bevy_ensemble::{AwaitingHost, AwaitingSeat, LobbyLeft, LobbyParticipant, Reconnectable};
use bevy_ticked::prelude::{TickHoldReason, TickHolds};
use bevy_ticked_networking::client::LastAppliedSeq;
use bevy_ticked_networking::server::{LastAck, SnapshotSeq};
use bevy_ticked_testing::fixtures::minimal::{self, Input, Owner, seat_everyone};
use bevy_ticked_testing::prelude::*;

const SETTLE: usize = 400;
/// Frames are a tick long in the harness: 64 to the second.
const SECOND: usize = 64;

const RECONNECT: Reconnectable = Reconnectable {
    within: Duration::from_secs(3),
};

/// A host and two clients, every lobby reconnectable, every body seated, and long enough played
/// that the host has numbered a few hundred snapshots to each client.
fn session() -> TickedNetwork {
    let mut net = TickedNetwork::client_server::<Input>(2, minimal::install)
        .with_link(Link::cable())
        .with_seed(5)
        .with_reconnection(RECONNECT);
    assert!(net.settle(SETTLE), "the session did not settle");
    seat_everyone(&mut net);
    net.run(5 * SECOND);
    net
}

fn holds(net: &TickedNetwork, peer: PeerId) -> TickHolds {
    net.app(peer).world().resource::<TickHolds>().clone()
}

fn awaiting(net: &mut TickedNetwork, peer: PeerId) -> Option<AwaitingHost> {
    let world = net.world_mut(peer);
    world.query::<&AwaitingHost>().iter(world).next().copied()
}

fn held_seats(net: &mut TickedNetwork, host: PeerId) -> Vec<u128> {
    let world = net.world_mut(host);
    world
        .query_filtered::<&LobbyParticipant, With<AwaitingSeat>>()
        .iter(world)
        .map(|participant| participant.player_uuid)
        .collect()
}

fn bodies(net: &mut TickedNetwork, peer: PeerId) -> Vec<u128> {
    let world = net.world_mut(peer);
    let mut owners: Vec<u128> = world
        .query::<&Owner>()
        .iter(world)
        .map(|owner| owner.0)
        .collect();
    owners.sort();
    owners
}

/// Every `LobbyLeft` a peer has written, counted as it is: a message is gone two frames later.
#[derive(Resource, Default)]
struct Left(usize);

fn count_left(mut left: MessageReader<LobbyLeft>, mut count: ResMut<Left>) {
    count.0 += left.read().count();
}

fn watch_leaving(net: &mut TickedNetwork, peer: PeerId) {
    net.app_mut(peer)
        .init_resource::<Left>()
        .add_systems(Update, count_left);
}

fn left(net: &TickedNetwork, peer: PeerId) -> usize {
    net.app(peer).world().resource::<Left>().0
}

#[test]
fn a_client_whose_link_broke_stands_still_and_resumes_in_place() {
    let mut net = session();
    let (host, client) = (net.host(), net.client());
    let uuid = net.uuid(client);
    let bodies_before = bodies(&mut net, host);
    watch_leaving(&mut net, client);

    net.break_link(client);
    net.run(2);
    let stopped_at = tick(net.app(client));
    net.run(SECOND);

    assert!(
        awaiting(&mut net, client).is_some_and(|waiting| waiting.is_reconnect()),
        "the client waits for its host to come back"
    );
    assert!(
        holds(&net, client).holds(TickHoldReason::AwaitingHost),
        "and its clock is held meanwhile: {:?}",
        holds(&net, client)
    );
    assert_eq!(
        tick(net.app(client)),
        stopped_at,
        "a client cut off from its host predicts nothing"
    );
    assert_eq!(
        held_seats(&mut net, host),
        [uuid],
        "the host holds its seat"
    );
    assert_eq!(
        bodies(&mut net, host),
        bodies_before,
        "and its body is still in the host's world"
    );
    assert_eq!(role(net.app(client)), Role::Client);

    net.reconnect(client);
    assert!(
        net.run_until(2 * SECOND, |net| !paused(net.app(net.client()))),
        "the client never ran again: {:?}",
        holds(&net, client)
    );
    net.run(SECOND);

    let target = target_replay_distance(net.app(client)).0 as i64;
    let lead = lead(net.app(client), net.app(host));
    assert!(
        (0..=target + 2).contains(&lead),
        "the client is back in step with the host: lead {lead}, target {target}"
    );
    assert!(
        applied_tick(net.app(client)).is_some_and(|applied| applied > stopped_at),
        "and applies the host's world again"
    );
    assert!(held_seats(&mut net, host).is_empty());
    assert_eq!(bodies(&mut net, host), bodies_before, "nobody lost a body");
    assert_eq!(bodies(&mut net, client), bodies_before);
    assert_eq!(left(&net, client), 0, "nobody left");
    assert_eq!(role(net.app(client)), Role::Client);
}

/// The host numbers a reconnected client's snapshots from one again. Nothing the client kept by
/// `seq` from the old connection may be acknowledged, or used to rebuild a delta, on the new one.
#[test]
fn nothing_numbered_on_the_old_connection_is_read_on_the_new_one() {
    let mut net = session();
    let (host, client) = (net.host(), net.client());
    let uuid = net.uuid(client);
    let old_seq = net.app(client).world().resource::<LastAppliedSeq>().0;
    assert!(
        old_seq.is_some_and(|seq| seq > 64),
        "the old connection numbered past the history window: {old_seq:?}"
    );

    net.break_link(client);
    net.run(SECOND / 2);
    net.reconnect(client);
    let unknown_before = replays(net.app(client)).dropped_unknown_baseline;
    // The other client keeps moving, so every snapshot after the keyframe is a delta.
    let mover = net.clients()[1];
    net.hold_input(mover, Input::RIGHT, 3 * SECOND);

    let sent = net
        .app(host)
        .world()
        .resource::<SnapshotSeq>()
        .0
        .get(&uuid)
        .copied();
    let acked = net
        .app(host)
        .world()
        .resource::<LastAck>()
        .0
        .get(&uuid)
        .copied();
    assert!(
        acked <= sent,
        "the client acknowledged {acked:?} on a connection that has sent up to {sent:?}"
    );
    assert!(
        sent.is_some_and(|sent| sent < old_seq.unwrap()),
        "the host numbered the new connection afresh: {sent:?}"
    );
    let unknown = replays(net.app(client)).dropped_unknown_baseline - unknown_before;
    assert!(
        unknown <= 1,
        "{unknown} deltas named a baseline the client did not hold: its bodies were the old \
         connection's"
    );
}

#[test]
fn a_client_that_is_not_reached_again_leaves() {
    let mut net = session();
    let client = net.client();
    watch_leaving(&mut net, client);
    net.break_link(client);
    net.run(4 * SECOND);

    assert_eq!(left(&net, client), 1, "the session ended for the client");
    assert_ne!(role(net.app(client)), Role::Client);
    assert!(
        !holds(&net, client).holds(TickHoldReason::AwaitingHost),
        "and nothing of the wait is left on its clock: {:?}",
        holds(&net, client)
    );
}
