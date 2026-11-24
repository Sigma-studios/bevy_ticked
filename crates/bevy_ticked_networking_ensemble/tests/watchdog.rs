//! A client the host's world stops reaching says so, asks for it, and stops saying so when it
//! comes back — and a host names the client it is no longer reaching.
//!
//! The silence is made by losing every unreliable packet from the host to the client, which is
//! every snapshot and nothing else: the client's inputs still reach the host, and the lobby's
//! reliable traffic still flows. That is the shape of the failure the watchdog was written for —
//! a client playing on alone with nothing to say it is.

use std::time::Duration;

use bevy_ticked_networking::client::AppliedSnapshotTick;
use bevy_ticked_networking::diagnostics::ReplayStats;
use bevy_ticked_networking::messages::ReceivedNetworkSnapshot;
use bevy_ticked_networking::pause::{PauseReason, PauseSession, ResumeSession};
use bevy_ticked_networking::snapshot::{
    DeltaBody, MARGIN_UNMEASURED, SnapshotBody, SnapshotPacket,
};
use bevy_ticked_networking::watchdog::{AckWatch, SnapshotWatchdog};
use bevy_ticked_networking_ensemble::EnsembleInputMessage;
use bevy_ticked_testing::fixtures::minimal::{self, Input, seat_everyone};
use bevy_ticked_testing::prelude::*;

const SETTLE: usize = 400;
/// Frames are a tick long in the harness: 64 to the second.
const SECOND: usize = 64;

fn session() -> TickedNetwork {
    let mut net = TickedNetwork::client_server::<Input>(1, minimal::install)
        .with_link(Link::cable())
        .with_seed(7);
    assert!(net.settle(SETTLE));
    seat_everyone(&mut net);
    net.run(60);
    net
}

fn starved_for(net: &TickedNetwork, peer: PeerId) -> Option<Duration> {
    net.app(peer)
        .world()
        .resource::<SnapshotWatchdog>()
        .starved_for()
}

fn received(net: &TickedNetwork, peer: PeerId) -> u64 {
    net.app(peer)
        .world()
        .resource::<ReplayStats>()
        .snapshots_received
}

fn unacknowledged(net: &TickedNetwork, host: PeerId) -> Vec<(u128, Duration)> {
    net.app(host)
        .world()
        .resource::<AckWatch>()
        .unacknowledged()
        .collect()
}

/// Every snapshot from the host to the client lost, until `restore`.
fn silence_snapshots(net: &mut TickedNetwork, host: PeerId, client: PeerId) {
    net.net
        .set_link_between(host, client, Link::cable().with_loss(1.0));
}

fn restore(net: &mut TickedNetwork, host: PeerId, client: PeerId) {
    net.net.set_link_between(host, client, Link::cable());
}

#[test]
fn a_client_the_host_stops_reaching_is_starved_and_recovers() {
    let mut net = session();
    let (host, client) = (net.host(), net.client());
    assert_eq!(
        starved_for(&net, client),
        None,
        "a healthy session is not starved"
    );

    net.trace_packets();
    silence_snapshots(&mut net, host, client);
    net.run(SECOND);
    assert_eq!(
        starved_for(&net, client),
        None,
        "one second without snapshots is not yet a starvation"
    );
    // Read after the silence has begun: whatever was already in flight still lands.
    let before = received(&net, client);
    // Two seconds to the starvation and its first request, and past the second a second later.
    net.run(2 * SECOND + SECOND / 2);
    assert_eq!(received(&net, client), before, "the link really was silent");
    let starved =
        starved_for(&net, client).expect("three and a half silent seconds starve the client");
    assert!(
        starved >= Duration::from_secs(2),
        "starved_for counts from the start of the silence, got {starved:?}"
    );

    // The nack went out on the client's input stream, which still works.
    let nacks = net
        .decode_messages::<EnsembleInputMessage<Input>>(client, host)
        .iter()
        .filter(|message| message.payload.nack_full)
        .count();
    assert!(
        nacks >= 2,
        "a starved client keeps asking for a full body, sent {nacks}"
    );

    restore(&mut net, host, client);
    net.run(SECOND / 2);
    assert_eq!(
        starved_for(&net, client),
        None,
        "the first snapshot through ends the starvation"
    );
}

#[test]
fn a_host_names_a_client_it_no_longer_reaches() {
    let mut net = session();
    let (host, client) = (net.host(), net.client());
    let uuid = net.uuid(client);

    silence_snapshots(&mut net, host, client);
    // The client must keep sending inputs for the host to call it playing.
    for _ in 0..3 * SECOND {
        queue_input(net.app_mut(client), uuid, Input::default());
        net.step();
    }
    let named = unacknowledged(&net, host);
    assert!(
        named.iter().any(|(who, _)| *who == uuid),
        "the host should name the client whose acks stopped, named {named:?}"
    );

    restore(&mut net, host, client);
    for _ in 0..SECOND {
        queue_input(net.app_mut(client), uuid, Input::default());
        net.step();
    }
    assert!(
        unacknowledged(&net, host).is_empty(),
        "an acknowledgement clears the report"
    );
}

#[test]
fn a_paused_host_does_not_starve_its_clients() {
    let mut net = session();
    let (host, client) = (net.host(), net.client());

    net.app_mut(host)
        .world_mut()
        .write_message(PauseSession(PauseReason::Host));
    net.run(5 * SECOND);
    assert_eq!(
        starved_for(&net, client),
        None,
        "a host that is paused still says so every half second"
    );
    assert!(unacknowledged(&net, host).is_empty());
    net.app_mut(host).world_mut().write_message(ResumeSession);
    net.run(SECOND);
    assert_eq!(starved_for(&net, client), None);
}

/// A repeat of the applied tick is the host's word only if it can be read. A delta against a
/// baseline this client no longer holds is dropped on arrival, and must not keep the client from
/// starving: it used to count as heard before it was rebuilt.
#[test]
fn a_repeat_that_cannot_be_rebuilt_does_not_keep_a_client_from_starving() {
    let mut net = session();
    let (host, client) = (net.host(), net.client());

    silence_snapshots(&mut net, host, client);
    for _ in 0..3 * SECOND + SECOND / 2 {
        let applied = net
            .app(client)
            .world()
            .resource::<AppliedSnapshotTick>()
            .0
            .expect("synced");
        let unreadable = SnapshotPacket {
            seq: u32::MAX,
            tick: applied,
            your_margin: MARGIN_UNMEASURED,
            body: SnapshotBody::Delta(DeltaBody {
                baseline_seq: u32::MAX - 1,
                ..Default::default()
            }),
        };
        net.app_mut(client)
            .world_mut()
            .trigger(ReceivedNetworkSnapshot(unreadable));
        net.run(1);
    }
    assert!(
        net.app(client)
            .world()
            .resource::<ReplayStats>()
            .dropped_unknown_baseline
            > 0,
        "the repeats really were unreadable"
    );
    assert!(
        starved_for(&net, client).is_some(),
        "repeats that cannot be rebuilt are not the host's word"
    );
}

#[test]
fn a_tab_in_the_background_is_not_a_starvation() {
    let mut net = session();
    let client = net.client();

    net.freeze(client, 5 * SECOND);
    for _ in 0..SECOND {
        net.step();
        assert_eq!(
            starved_for(&net, client),
            None,
            "coming back from five seconds away, with the packets that arrived meanwhile"
        );
    }
}
