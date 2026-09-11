//! What the host accepts from a client, and from whom.
//!
//! Everything a client sends the host used to be taken at face value: an action for a tick
//! already simulated was merged into the record of it, a tick a million ahead reserved memory
//! for as long as it took to get there, a second `ClientLoaded` reopened the sender's grace
//! window, a join snapshot request was a way to make the host walk its world, and a checksum
//! report from anybody could switch the desync checker off for the whole session. None of it
//! needed a hostile client; the late action happens on every join. Each test here hands the
//! host one such message — crafted with `deliver_raw`, so the test is about the host and not
//! about whether a client would send it — and checks what the host did with it.

mod common;

use bevy::prelude::*;
use bevy_ensemble_loopback::{Link, LoopbackNetwork, PeerId};
use bevy_ticked_lockstep_networking::testing::{actions_at, participant_joined_at, tracked_ticks};
use bevy_ticked_lockstep_networking::{
    ChecksumReport, ClientLoaded, ClientScheduledActions, Desync, JoinSnapshotRequest,
    PendingClientJoins,
};
use bevy_ticked_testing::fault::{bit_flips, encode_as, garbage, truncations};
use common::*;
use std::time::Duration;

const HOST_PEER: PeerId = PeerId(0);
const CLIENT_PEER: PeerId = PeerId(1);
const CLIENT: u128 = 2;

/// A message the host would receive from `sender`, exactly as its transport would hand it over.
fn deliver<T: bevy_ensemble::EnsembleMessage>(
    net: &mut LoopbackNetwork,
    sender: u128,
    message: &T,
) {
    let bytes = encode_as(net.app(HOST_PEER), message);
    net.deliver_raw(HOST_PEER, sender, bytes);
}

fn pending_on_host(net: &LoopbackNetwork) -> Vec<u128> {
    net.app(HOST_PEER)
        .world()
        .resource::<PendingClientJoins>()
        .0
        .keys()
        .copied()
        .collect()
}

fn desync_on_host(net: &LoopbackNetwork) -> Option<Desync<CounterHash>> {
    net.app(HOST_PEER)
        .world()
        .get_resource::<Desync<CounterHash>>()
        .cloned()
}

fn snapshots_captured(net: &LoopbackNetwork) -> u32 {
    net.app(HOST_PEER).world().resource::<SnapshotsCaptured>().0
}

/// A host, a participant, and a second client that connected — its `LobbyClient` exists and
/// is verified — but never loaded, so it is on nobody's roster.
fn host_participant_and_bystander() -> LoopbackNetwork {
    let mut net = joined_pair();
    net.add_client(
        3,
        peer_with(
            3,
            Recipe {
                finishes_join: false,
                ..Recipe::default()
            },
        ),
    );
    net.run(100);
    assert!(
        participant_joined_at(net.app(HOST_PEER), 3).is_none(),
        "the bystander must not be a participant, or this tests nothing"
    );
    net
}

// ── Late and far-future actions ──────────────────────────────────────────────

/// Three peers, because that is how many it takes to see the damage. A late action merged into
/// a simulated tick changed the host's record of it; the host and the client that sent it had
/// simulated the tick without it; and the *next* client to join was caught up from the record,
/// with it — and disagreed with both of them from then on.
#[test]
fn a_late_client_action_for_a_simulated_tick_is_dropped_not_merged() {
    let mut net = joined_pair();
    let newcomer = net.add_client(3, peer(3));
    // Slow enough that the newcomer's join is pending for a while, which is the window in which
    // the host keeps simulated ticks for its catch-up.
    net.set_link_pair(
        newcomer,
        HOST_PEER,
        Link::delayed(Duration::from_millis(100)),
        Link::delayed(Duration::from_millis(100)),
    );
    run_until(
        &mut net,
        100,
        "the newcomer's join becoming pending",
        |net| pending_on_host(net).contains(&3),
    );
    net.run(2);

    let simulated = tick(&net, HOST_PEER) - 1;
    deliver(
        &mut net,
        CLIENT,
        &ClientScheduledActions::<Action> {
            tick: simulated,
            actions: vec![5],
        },
    );
    net.run(2);

    assert!(
        tracked_ticks::<Action>(net.app(HOST_PEER)).contains(&simulated),
        "the host keeps tick {simulated} for the pending join; if it does not, the assertion \
         below is vacuous"
    );
    assert!(
        !actions_at::<Action>(net.app(HOST_PEER), simulated).contains(&5),
        "the late action was merged into a tick the host had already simulated and broadcast"
    );

    run_until(
        &mut net,
        400,
        "the newcomer becoming a participant",
        |net| participant_joined_at(net.app(HOST_PEER), 3).is_some(),
    );
    net.run(300);
    for client in [CLIENT_PEER, newcomer] {
        assert!(logs_overlap(&net, HOST_PEER, client, 50));
        assert_eq!(
            first_divergence(&net, HOST_PEER, client),
            None,
            "peer {client:?} disagrees with the host"
        );
    }
}

#[test]
fn a_client_scheduling_for_a_far_future_tick_is_ignored() {
    let mut net = joined_pair();
    let now = tick(&net, HOST_PEER);
    let far = now + 10_000;
    let near = now + 100;

    deliver(
        &mut net,
        CLIENT,
        &ClientScheduledActions::<Action> {
            tick: far,
            actions: vec![1],
        },
    );
    deliver(
        &mut net,
        CLIENT,
        &ClientScheduledActions::<Action> {
            tick: near,
            actions: vec![1],
        },
    );
    net.run(2);

    let tracked = tracked_ticks::<Action>(net.app(HOST_PEER));
    assert!(
        !tracked.contains(&far),
        "a tick ten thousand ahead is a tracker entry kept for ten thousand ticks"
    );
    assert!(
        tracked.contains(&near),
        "a tick inside the horizon is an ordinary early batch and must still be recorded"
    );
}

#[test]
fn pending_joins_are_forgotten_when_the_client_disconnects() {
    let mut net = LoopbackNetwork::new(TICK);
    net.add_host(HOST, peer(HOST));
    net.run(50);
    let client = net.add_client(CLIENT, peer(CLIENT));
    net.set_link_pair(
        client,
        HOST_PEER,
        Link::delayed(Duration::from_millis(200)),
        Link::delayed(Duration::from_millis(200)),
    );

    run_until(&mut net, 100, "the join becoming pending", |net| {
        pending_on_host(net).contains(&CLIENT)
    });
    net.disconnect(client);
    net.run(2);

    assert!(
        pending_on_host(&net).is_empty(),
        "a join that will never finish keeps the tracker's floor for the rest of the session"
    );
}

// ── Repeats ──────────────────────────────────────────────────────────────────

#[test]
fn a_repeated_join_snapshot_request_is_answered_once() {
    let mut net = LoopbackNetwork::new(TICK);
    net.add_host(HOST, peer(HOST));
    net.run(50);
    let client = net.add_client(CLIENT, peer(CLIENT));
    net.set_link_pair(
        client,
        HOST_PEER,
        Link::delayed(Duration::from_millis(100)),
        Link::delayed(Duration::from_millis(100)),
    );
    // Two more on top of the one the client sends itself, all inside a second.
    deliver(&mut net, CLIENT, &JoinSnapshotRequest);
    deliver(&mut net, CLIENT, &JoinSnapshotRequest);

    run_until(&mut net, 200, "the client becoming a participant", |net| {
        participant_joined_at(net.app(HOST_PEER), CLIENT).is_some()
    });
    assert_eq!(
        snapshots_captured(&net),
        1,
        "each request captured a snapshot, and each snapshot the client applied wiped the \
         ticks that had arrived since the last"
    );

    // Past the interval, and from a participant: not a retry, and not answered either.
    net.run(70);
    deliver(&mut net, CLIENT, &JoinSnapshotRequest);
    net.run(5);
    assert_eq!(
        snapshots_captured(&net),
        1,
        "a participant that asks for a snapshot has loaded already; a second one cannot help it"
    );
}

#[test]
fn a_repeated_client_loaded_does_not_reset_the_grace_window() {
    let mut net = joined_pair();
    let joined_at = participant_joined_at(net.app(HOST_PEER), CLIENT).expect("joined");

    deliver(&mut net, CLIENT, &ClientLoaded { buffer: 6 });
    net.run(5);

    assert_eq!(
        participant_joined_at(net.app(HOST_PEER), CLIENT),
        Some(joined_at),
        "a client could keep its grace window open, and its actions optional, for as long as \
         it kept saying it had loaded"
    );
}

// ── Who is heard ─────────────────────────────────────────────────────────────

#[test]
fn a_checksum_report_from_a_non_participant_does_not_latch_desync() {
    let mut net = host_participant_and_bystander();
    let (sampled_tick, hash) = log(&net, HOST_PEER).latest().expect("the host samples");
    let wrong = CounterHash {
        counter: hash.counter + 999,
    };

    deliver(
        &mut net,
        3,
        &ChecksumReport {
            tick: sampled_tick,
            hash: wrong,
        },
    );
    net.run(5);
    assert!(
        desync_on_host(&net).is_none(),
        "a peer that is simulating nothing switched the checker off for the whole session"
    );

    // The same report from the participant is a real disagreement, confirmed against the
    // host's own sample — so the test is about the sender and not about the report.
    deliver(
        &mut net,
        CLIENT,
        &ChecksumReport {
            tick: sampled_tick,
            hash: wrong,
        },
    );
    net.run(5);
    let desync = desync_on_host(&net).expect("a participant's disagreement is reported");
    assert_eq!(desync.peer, CLIENT);
    assert_eq!(desync.divergence.tick, sampled_tick);
}

/// A report for a tick the host has not sampled is not parked on the host: the host is behind
/// nobody, so such a tick does not exist yet, and a report for it is a report the host can
/// never confirm.
#[test]
fn a_report_for_a_tick_the_host_has_not_sampled_is_ignored() {
    let mut net = joined_pair();
    let future = tick(&net, HOST_PEER) + 20;
    deliver(
        &mut net,
        CLIENT,
        &ChecksumReport {
            tick: future,
            hash: CounterHash { counter: 0 },
        },
    );
    run_until_tick(&mut net, HOST_PEER, future + 5, 60);

    assert!(
        desync_on_host(&net).is_none(),
        "the host parked a client's report and latched on it once its own clock got there"
    );
}

#[test]
fn actions_from_a_non_participant_are_dropped() {
    let mut net = host_participant_and_bystander();
    let target = tick(&net, HOST_PEER) + 3;

    deliver(
        &mut net,
        3,
        &ClientScheduledActions::<Action> {
            tick: target,
            actions: vec![7],
        },
    );
    net.run(1);
    assert!(
        !actions_at::<Action>(net.app(HOST_PEER), target).contains(&7),
        "an action from a peer on nobody's roster was recorded, to be broadcast under a uuid no \
         client knows"
    );

    deliver(
        &mut net,
        CLIENT,
        &ClientScheduledActions::<Action> {
            tick: target,
            actions: vec![7],
        },
    );
    net.run(1);
    assert!(
        actions_at::<Action>(net.app(HOST_PEER), target).contains(&7),
        "the same message from a participant is an ordinary batch"
    );
}

// ── Bytes from the network ───────────────────────────────────────────────────

/// A decoder never panics on bytes from the network. A malformed packet is logged and skipped;
/// a panic on one is a remote crash for every peer that receives it.
#[test]
fn truncated_lockstep_actions_never_panic() {
    let mut net = joined_pair();
    let packet = encode_as(
        net.app(HOST_PEER),
        &ClientScheduledActions::<Action> {
            tick: tick(&net, HOST_PEER) + 3,
            actions: vec![1, 2, 3],
        },
    );

    let mut mutants: Vec<Vec<u8>> = truncations(&packet).map(<[u8]>::to_vec).collect();
    mutants.extend(bit_flips(&packet, 7, 64));
    mutants.extend(garbage(11, 32, packet.len() * 2));
    for mutant in mutants {
        net.deliver_raw(HOST_PEER, CLIENT, mutant);
        net.step();
    }

    // Still a host. Whether the client survived what its packets decoded as is the transport's
    // business; the host did not crash and did not stop.
    let before = tick(&net, HOST_PEER);
    net.run(100);
    assert!(
        tick(&net, HOST_PEER) > before + 50,
        "the host stopped advancing after being fed malformed packets"
    );
}
