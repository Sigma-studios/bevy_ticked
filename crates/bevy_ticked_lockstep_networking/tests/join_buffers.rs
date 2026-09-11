//! The join, when the two peers do not agree about how far ahead to schedule.
//!
//! `host_tick_buffer` and `client_tick_buffer` are separate numbers on separate peers, and the
//! adaptive tuner moves each from what it measures on its own side. Nothing about the join
//! used to take the difference into account: the host sized the joiner's grace window from
//! the host's buffer, the joiner started its scheduled sequence from wherever its own buffer
//! put it, and when the joiner's was the larger the two never met. Every test here is a session
//! that used to stop dead at the join, or shortly after, and now does not.

mod common;

use bevy::prelude::*;
use bevy_ensemble_loopback::{Link, LoopbackNetwork, PeerId};
use bevy_ticked_lockstep_networking::testing::{
    actions_at, client_tick_buffer, host_tick_buffer, participant_joined_at, push_action,
};
use bevy_ticked_lockstep_networking::{AdaptiveBufferState, LockstepConfig};
use common::*;

const HOST_PEER: PeerId = PeerId(0);
const CLIENT_PEER: PeerId = PeerId(1);

fn config(host_tick_buffer: u64, client_tick_buffer: u64) -> LockstepConfig {
    LockstepConfig {
        host_tick_buffer,
        client_tick_buffer,
        ..default()
    }
}

/// Both peers simulate on, and agree about what they simulated.
fn assert_session_runs(net: &mut LoopbackNetwork, frames: usize) {
    let host_was = tick(net, HOST_PEER);
    let client_was = tick(net, CLIENT_PEER);
    net.run(frames);
    let wanted = frames as u64 / 2;
    assert!(
        tick(net, HOST_PEER) >= host_was + wanted,
        "the host advanced {} ticks in {frames} frames: the session is stalled",
        tick(net, HOST_PEER) - host_was
    );
    assert!(
        tick(net, CLIENT_PEER) >= client_was + wanted,
        "the client advanced {} ticks in {frames} frames: the session is stalled",
        tick(net, CLIENT_PEER) - client_was
    );
    assert!(
        logs_overlap(net, HOST_PEER, CLIENT_PEER, 50),
        "the two logs share too few ticks for their agreement to mean anything"
    );
    assert_eq!(
        first_divergence(net, HOST_PEER, CLIENT_PEER),
        None,
        "the peers ran and disagreed"
    );
}

/// The host buffers four ticks; the joiner, forty. The joiner's first scheduled tick used to
/// land past the first tick the host required of it, and the host waited for the ticks in
/// between for ever.
#[test]
fn a_join_with_mismatched_buffers_does_not_deadlock() {
    let mut net = LoopbackNetwork::new(TICK);
    net.add_host(HOST, peer_with(HOST, Recipe::with_config(config(4, 4))));
    net.add_client(2, peer_with(2, Recipe::with_config(config(40, 40))));

    net.run(100);
    assert_session_runs(&mut net, 400);
}

/// A player whose buffer the tuner grew to forty-eight on a satellite link, joining a LAN host
/// whose buffer is four. The joiner tells the host what it is scheduling with, and the host
/// sizes the grace window from the larger of the two.
#[test]
fn a_client_carrying_a_large_buffer_from_a_previous_session_can_join_a_lan_host() {
    let mut net = LoopbackNetwork::new(TICK);
    net.add_host(HOST, peer_with(HOST, Recipe::with_config(config(4, 4))));
    net.run(50);

    let mut joiner = peer(2);
    // What the tuner leaves behind: not the config the plugin was built with.
    joiner
        .world_mut()
        .resource_mut::<LockstepConfig>()
        .client_tick_buffer = 48;
    let host_tick_when_added = tick(&net, HOST_PEER);
    net.add_client(2, joiner);

    run_until(&mut net, 200, "the client becoming a participant", |net| {
        participant_joined_at(net.app(HOST_PEER), 2).is_some()
    });
    let joined_at = participant_joined_at(net.app(HOST_PEER), 2).expect("joined");
    assert!(
        joined_at >= host_tick_when_added + 1 + 48,
        "joined_at_tick {joined_at} leaves the joiner less than its own 48-tick buffer to get \
         its first actions in (the host was at {host_tick_when_added} when it connected)"
    );

    assert_session_runs(&mut net, 400);
}

/// What the tuner learned about one link is not true of the next. The buffer and the estimate
/// go back to what the plugin was built with when the lobby goes.
#[test]
fn adaptive_state_is_reset_with_the_lobby() {
    let recipe = Recipe {
        adaptive: true,
        ..Recipe::default()
    };
    let mut net = LoopbackNetwork::new(TICK);
    net.add_host(HOST, peer_with(HOST, recipe));
    net.add_client(2, peer_with(2, recipe));
    net.set_link(Link::satellite());

    run_until(
        &mut net,
        500,
        "the tuner growing the client's buffer",
        |net| client_tick_buffer(net.app(CLIENT_PEER)) > 6,
    );
    assert!(
        net.app(CLIENT_PEER)
            .world()
            .resource::<AdaptiveBufferState>()
            .rtt_estimate()
            .is_some(),
        "the tuner has an estimate while the session runs"
    );

    net.leave(CLIENT_PEER);
    net.run(3);

    let client = net.app(CLIENT_PEER);
    assert_eq!(
        client_tick_buffer(client),
        6,
        "the buffer the satellite link grew is still in force in a lobby that no longer exists"
    );
    assert_eq!(
        host_tick_buffer(client),
        6,
        "the host-side buffer is part of the same config and goes back with it"
    );
    assert_eq!(
        client
            .world()
            .resource::<AdaptiveBufferState>()
            .rtt_estimate(),
        None,
        "an estimate of a link that is gone would be averaged into the next session's first \
         samples"
    );
}

/// A host buffer of zero is clamped to one, and the host's own actions never gate its advance:
/// the two changes that between them turn a session that never started into one that runs.
#[test]
fn host_tick_buffer_zero_does_not_deadlock() {
    let mut net = LoopbackNetwork::new(TICK);
    net.add_host(HOST, peer_with(HOST, Recipe::with_config(config(0, 6))));
    net.add_client(2, peer(2));

    assert_eq!(
        host_tick_buffer(net.app(HOST_PEER)),
        1,
        "a zero grace window asks a client's first batch to arrive before it can have been sent"
    );
    net.run(100);
    assert_session_runs(&mut net, 300);
}

/// The host's actions go into the tick about to run. A client's have to cross the link and be
/// ruled on first; the host's do not cross anything, and buffering them was pure input lag.
#[test]
fn the_host_pays_no_input_lag_for_its_own_actions() {
    let mut net = joined_pair();
    let before = tick(&net, HOST_PEER);
    let counter_before = counter(&net, HOST_PEER);

    push_action::<Action>(net.app_mut(HOST_PEER), 3);
    run_until_tick(&mut net, HOST_PEER, before + 1, 10);

    assert_eq!(
        actions_at::<Action>(net.app(HOST_PEER), before + 1),
        vec![3],
        "the action was scheduled for some later tick, or lost"
    );
    assert_eq!(
        counter(&net, HOST_PEER),
        counter_before + 1 + 3,
        "tick {} ran without the action pushed before it",
        before + 1
    );

    // And the client applied it at the same tick, from the authoritative broadcast.
    net.run(100);
    assert!(logs_overlap(&net, HOST_PEER, CLIENT_PEER, 50));
    assert_eq!(first_divergence(&net, HOST_PEER, CLIENT_PEER), None);
}
