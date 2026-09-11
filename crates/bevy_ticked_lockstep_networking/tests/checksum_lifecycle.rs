//! What the checksum log holds across a join and a leave.
//!
//! A hash is a fact about one world at one tick number, and a peer's tick numbers are not its
//! own: a join snapshot moves its clock to the host's, and the next session starts again from
//! the numbers this one used. Samples that outlive the world they describe are compared
//! against the next world's at the same numbers, and the exchange reports the difference as a
//! desync — at the first tick the two logs happen to share, which is usually the very first
//! comparison after the join.

mod common;

use bevy_ensemble_loopback::{LoopbackNetwork, PeerId};
use common::*;

const HOST_PEER: PeerId = PeerId(0);
const CLIENT_PEER: PeerId = PeerId(1);

/// A host that has been running for a while, and a client that ran alone — a menu, a warm-up,
/// a single-player round — for a while of its own, sampling all along, before joining.
///
/// The idler's world is set apart on purpose: two counters that both start at zero and both
/// add one per tick are the *same* world, and a test that could not tell the logs apart would
/// pass whether or not the join cleared anything.
fn host_and_idler() -> LoopbackNetwork {
    let mut net = LoopbackNetwork::new(TICK);
    net.add_host(HOST, peer(HOST));
    net.run(500);

    let mut idler = peer_with(
        2,
        Recipe {
            sample_without_lobby: true,
            ..Recipe::default()
        },
    );
    idler.world_mut().resource_mut::<Counter>().0 = 1_000;
    for _ in 0..300 {
        idler.update();
    }
    assert!(
        log_len(&idler) >= 250,
        "the idler sampled while alone, or this proves nothing"
    );

    net.add_client(2, idler);
    net
}

fn log_len(app: &bevy::prelude::App) -> usize {
    app.world()
        .resource::<bevy_ticked_lockstep_networking::ChecksumLog<CounterHash>>()
        .samples
        .len()
}

#[test]
fn a_player_who_idled_in_a_menu_does_not_desync_on_join() {
    let mut net = host_and_idler();
    net.run(300);

    assert!(
        logs_overlap(&net, HOST_PEER, CLIENT_PEER, 100),
        "the joiner has to be simulating the same ticks as the host for agreement to mean \
         anything"
    );
    assert_eq!(
        first_divergence(&net, HOST_PEER, CLIENT_PEER),
        None,
        "the joiner's samples of the world it ran alone were compared against the host's, at \
         tick numbers both had used, and called a desync"
    );
    assert!(
        net.app(CLIENT_PEER)
            .world()
            .get_resource::<bevy_ticked_lockstep_networking::Desync<CounterHash>>()
            .is_none(),
        "the exchange latched on the pre-join samples"
    );
}

#[test]
fn the_checksum_log_is_cleared_on_join_and_on_leave() {
    let mut net = host_and_idler();
    net.run(200);

    // The idler's own samples were at ticks 1..=300; the host was past 500 when it sent the
    // snapshot, so every sample that survived the join is of the joined world.
    let oldest = log(&net, CLIENT_PEER)
        .oldest()
        .map(|(tick, _)| tick)
        .expect("the client is sampling the joined world");
    assert!(
        oldest > 300,
        "the log still holds a sample from tick {oldest}, which is the world the client ran \
         alone, not the one it joined"
    );

    let left_at = tick(&net, CLIENT_PEER);
    net.leave(CLIENT_PEER);
    net.run(3);

    let samples = &log(&net, CLIENT_PEER).samples;
    assert!(
        samples.len() <= 3,
        "{} samples survived the leave; the next session starts at tick numbers they hold",
        samples.len()
    );
    assert!(
        samples.iter().all(|(tick, _)| *tick > left_at),
        "a sample from the session that ended is still in the log: {samples:?}"
    );
}
