//! Lockstep part 2: the session's own actions on the tick, the stall, catching up, and a
//! buffer sized from the host's word rather than the pings.

mod common;

use std::time::Duration;

use bevy::prelude::*;
use bevy::time::TimeUpdateStrategy;
use bevy_ensemble_loopback::{Link, LoopbackNetwork, PeerId};
use bevy_ticked::prelude::{TickHoldReason, TickHolds, TickRateDilation, TickedEvents};
use bevy_ticked_lockstep_networking::{
    LockstepPauseReason, LockstepPaused, LockstepRoster, LockstepStall, OwnInputMargin,
    PauseLockstep, ResumeLockstep, RosterChange, StallPolicy,
};
use common::*;

const HOST_PEER: PeerId = PeerId(0);
const A: PeerId = PeerId(1);
const B: PeerId = PeerId(2);

/// A host and two joined clients, running.
fn trio() -> LoopbackNetwork {
    let mut net = LoopbackNetwork::new(TICK);
    net.add_host(HOST, peer(HOST));
    net.add_client(2, peer(2));
    net.add_client(3, peer(3));
    net.run(300);
    for p in [HOST_PEER, A, B] {
        assert!(tick(&net, p) > 100, "peer {p:?} is not running");
    }
    net
}

fn roster(net: &LoopbackNetwork, p: PeerId) -> Vec<u128> {
    net.app(p)
        .world()
        .resource::<LockstepRoster>()
        .0
        .iter()
        .copied()
        .collect()
}

fn stall(net: &LoopbackNetwork, p: PeerId) -> LockstepStall {
    net.app(p).world().resource::<LockstepStall>().clone()
}

/// The tick a peer saw `uuid` leave on, from its roster events.
fn left_at(net: &LoopbackNetwork, p: PeerId, uuid: u128) -> Option<u64> {
    let events = net.app(p).world().resource::<TickedEvents<RosterChange>>();
    let newest = tick(net, p);
    (0..=newest).find(|t| events.at_tick(*t).contains(&RosterChange::Left(uuid)))
}

fn joined_at(net: &LoopbackNetwork, p: PeerId, uuid: u128) -> Option<u64> {
    let events = net.app(p).world().resource::<TickedEvents<RosterChange>>();
    let newest = tick(net, p);
    (0..=newest).find(|t| events.at_tick(*t).contains(&RosterChange::Joined(uuid)))
}

#[test]
fn the_roster_changes_on_the_same_tick_everywhere() {
    let net = trio();
    for p in [HOST_PEER, A, B] {
        assert_eq!(
            roster(&net, p),
            vec![1, 2, 3],
            "peer {p:?} has the whole roster"
        );
    }
    for uuid in [2u128, 3] {
        let on_host = joined_at(&net, HOST_PEER, uuid).expect("the host saw the join");
        assert_eq!(
            joined_at(&net, A, uuid),
            Some(on_host),
            "A saw {uuid} join on the host's tick"
        );
        assert_eq!(
            joined_at(&net, B, uuid),
            Some(on_host),
            "B saw {uuid} join on the host's tick"
        );
    }
}

#[test]
fn an_unresponsive_participant_pauses_the_session_then_is_kicked() {
    let mut net = trio();
    net.app_mut(HOST_PEER).insert_resource(StallPolicy {
        pause_after: Duration::from_secs(1),
        kick_after: Some(Duration::from_secs(2)),
    });
    let host_was = tick(&net, HOST_PEER);

    // B stops running: no frames, no actions.
    for _ in 0..40 {
        net.step_only(&[HOST_PEER, A]);
    }
    assert_eq!(
        stall(&net, HOST_PEER).waiting_on,
        vec![3],
        "the host says who it waits on"
    );
    assert!(!stall(&net, HOST_PEER).paused, "not yet a pause");
    assert!(
        tick(&net, HOST_PEER) < host_was + 40,
        "the session stalled: {} -> {}",
        host_was,
        tick(&net, HOST_PEER)
    );
    assert_eq!(
        stall(&net, A).waiting_on,
        vec![1],
        "a client waits on its host"
    );

    for _ in 0..40 {
        net.step_only(&[HOST_PEER, A]);
    }
    assert!(
        stall(&net, HOST_PEER).paused,
        "a second in, it is reported as a pause"
    );

    for _ in 0..80 {
        net.step_only(&[HOST_PEER, A]);
    }
    // Kicked: the session runs again for the host and A, and B is gone from both rosters on
    // the same tick.
    let resumed_at = tick(&net, HOST_PEER);
    for _ in 0..64 {
        net.step_only(&[HOST_PEER, A]);
    }
    assert!(
        tick(&net, HOST_PEER) > resumed_at + 40,
        "the session runs again"
    );
    assert_eq!(roster(&net, HOST_PEER), vec![1, 2]);
    assert_eq!(roster(&net, A), vec![1, 2]);
    assert!(stall(&net, HOST_PEER).waiting_on.is_empty());
    assert_eq!(
        first_divergence(&net, HOST_PEER, A),
        None,
        "the survivors agree"
    );
}

#[test]
fn a_kicked_participant_leaves_on_the_same_tick_everywhere() {
    let mut net = trio();
    net.app_mut(HOST_PEER).insert_resource(StallPolicy {
        pause_after: Duration::from_millis(200),
        kick_after: Some(Duration::from_millis(500)),
    });
    for _ in 0..200 {
        net.step_only(&[HOST_PEER, A]);
    }
    let on_host = left_at(&net, HOST_PEER, 3).expect("the host saw B leave");
    assert_eq!(
        left_at(&net, A, 3),
        Some(on_host),
        "A saw B leave on the same tick"
    );
}

#[test]
fn a_join_does_not_freeze_the_session_longer_than_the_snapshot_transfer() {
    let mut net = LoopbackNetwork::new(TICK);
    net.add_host(HOST, peer(HOST));
    net.add_client(2, peer(2));
    net.run(200);
    let host_was = tick(&net, HOST_PEER);
    net.add_client(3, peer(3));
    net.run(120);
    let advanced = tick(&net, HOST_PEER) - host_was;
    assert!(
        advanced >= 100,
        "the host advanced {advanced} ticks in 120 frames while a client joined: it froze"
    );
    assert_eq!(roster(&net, HOST_PEER), vec![1, 2, 3]);
}

#[test]
fn a_joiner_fast_forwards_to_the_host() {
    // A slow link, so the join takes long enough to leave a backlog worth catching up on.
    let mut net = LoopbackNetwork::new(TICK);
    net.set_link(Link::satellite());
    net.add_host(HOST, peer(HOST));
    net.add_client(2, peer(2));
    net.run(400);
    net.add_client(3, peer(3));
    // The join hands B a burst of ticks; it runs fast through them.
    let mut sped_up = false;
    let mut caught_up_at = None;
    for frame in 0..400 {
        net.step();
        let dilation = net.app(B).world().resource::<TickRateDilation>().0;
        if dilation > 1.0 {
            sped_up = true;
        }
        let behind = tick(&net, HOST_PEER).saturating_sub(tick(&net, B));
        // On a 300 ms one-way link a client can only ever be a round trip behind.
        if caught_up_at.is_none() && tick(&net, B) > 50 && behind <= 48 {
            caught_up_at = Some(frame);
        }
    }
    assert!(sped_up, "the joiner never ran faster than real time");
    let caught_up_at = caught_up_at.expect("the joiner never caught up with the host");
    println!("caught up {caught_up_at} frames after joining");
    assert!(caught_up_at < 300);
    assert_eq!(first_divergence(&net, HOST_PEER, B), None);
}

#[test]
fn the_buffer_follows_the_reliable_stream_not_the_pings() {
    let mut net = LoopbackNetwork::new(TICK);
    net.add_host(HOST, peer(HOST));
    net.add_client(2, peer(2));
    net.run(200);
    let margin = net.app(A).world().resource::<OwnInputMargin>().0;
    assert!(
        margin.is_some(),
        "the host reports the client's arrival margin"
    );
    let buffer_before = net
        .app(A)
        .world()
        .resource::<bevy_ticked_lockstep_networking::LockstepConfig>()
        .client_tick_buffer;

    // A far slower link: the client's batches start arriving late and its buffer grows,
    // from the host's word, with no ping measurement in the loop.
    net.set_link(Link::satellite());
    net.run(400);
    let buffer_after = net
        .app(A)
        .world()
        .resource::<bevy_ticked_lockstep_networking::LockstepConfig>()
        .client_tick_buffer;
    assert!(
        buffer_after > buffer_before + 10,
        "a 600 ms round trip needs far more than {buffer_before} ticks of buffer: {buffer_after}"
    );
    let margin = net.app(A).world().resource::<OwnInputMargin>().0.unwrap();
    assert!(
        margin >= 0,
        "and its batches arrive in time again: margin {margin}"
    );
    assert_eq!(first_divergence(&net, HOST_PEER, A), None);
}

#[test]
fn a_frozen_host_in_lockstep_pauses_clients_then_resumes_in_agreement() {
    let mut net = trio();
    let a_was = tick(&net, A);
    for _ in 0..100 {
        net.step_only(&[A, B]);
    }
    assert!(
        net.app(A)
            .world()
            .resource::<TickHolds>()
            .holds(TickHoldReason::WaitingForPeers),
        "a client without authoritative ticks waits"
    );
    assert!(tick(&net, A) <= a_was + 8, "and does not run ahead");
    assert_eq!(stall(&net, A).waiting_on, vec![1]);
    assert!(stall(&net, A).paused);

    net.run(200);
    assert!(!net.app(A).world().resource::<TickHolds>().is_held());
    assert!(stall(&net, A).waiting_on.is_empty());
    assert_eq!(first_divergence(&net, HOST_PEER, A), None);
    assert_eq!(first_divergence(&net, HOST_PEER, B), None);
}

#[test]
fn lockstep_pause_composes_with_a_manual_hold() {
    let mut net = trio();
    net.app_mut(HOST_PEER)
        .world_mut()
        .write_message(PauseLockstep(LockstepPauseReason::Host));
    net.run(30);
    for p in [HOST_PEER, A, B] {
        assert_eq!(
            net.app(p).world().resource::<LockstepPaused>().0,
            Some(LockstepPauseReason::Host),
            "peer {p:?} knows the session is paused"
        );
    }
    let host_paused_at = tick(&net, HOST_PEER);
    net.run(30);
    assert_eq!(tick(&net, HOST_PEER), host_paused_at, "the host holds");
    assert_eq!(
        tick(&net, A),
        host_paused_at,
        "and so does every client, at the same tick"
    );

    // A's player opens a menu during the pause.
    net.app_mut(A)
        .world_mut()
        .resource_mut::<TickHolds>()
        .hold(TickHoldReason::Manual);
    net.app_mut(HOST_PEER)
        .world_mut()
        .write_message(ResumeLockstep);
    net.run(60);
    // The host runs the resume tick and the few ticks A scheduled before the pause, then
    // waits on A: lockstep is only as fast as its slowest participant, and A's menu is open.
    assert!(
        tick(&net, HOST_PEER) > host_paused_at,
        "the host ran the resume tick"
    );
    assert_eq!(
        net.app(HOST_PEER).world().resource::<LockstepPaused>().0,
        None,
        "the session is not paused any more"
    );
    assert_eq!(
        stall(&net, HOST_PEER).waiting_on,
        vec![2],
        "it waits on A now"
    );
    assert!(
        tick(&net, A) <= host_paused_at + 2,
        "A's menu is still open: {}",
        tick(&net, A)
    );
    net.app_mut(A)
        .world_mut()
        .resource_mut::<TickHolds>()
        .release(TickHoldReason::Manual);
    net.run(100);
    assert!(tick(&net, A) > host_paused_at + 40);
    assert_eq!(
        net.app(A).world().resource::<LockstepPaused>().0,
        None,
        "A applied the resume tick"
    );
    assert_eq!(first_divergence(&net, HOST_PEER, A), None);
}

/// bevy_factory's port: a peer whose frames sometimes carry two ticks agrees with one whose
/// frames carry one.
#[test]
fn uneven_frame_pacing_between_peers_keeps_them_in_agreement() {
    let mut net = trio();
    for frame in 0..200 {
        // Every third frame, A runs two ticks' worth of time.
        let long = frame % 3 == 0;
        net.app_mut(A)
            .insert_resource(TimeUpdateStrategy::ManualDuration(if long {
                TICK * 2
            } else {
                TICK
            }));
        net.step();
    }
    net.app_mut(A)
        .insert_resource(TimeUpdateStrategy::ManualDuration(TICK));
    net.run(100);
    assert!(logs_overlap(&net, HOST_PEER, A, 100));
    assert_eq!(first_divergence(&net, HOST_PEER, A), None);
    assert_eq!(first_divergence(&net, HOST_PEER, B), None);
}

/// A frame that runs several ticks runs them exactly as separate frames would have.
#[test]
fn a_frame_that_runs_several_ticks_does_not_change_the_world() {
    let mut net = trio();
    net.run(50);
    let mut steady = LoopbackNetwork::new(TICK);
    steady.add_host(HOST, peer(HOST));
    steady.add_client(2, peer(2));
    steady.add_client(3, peer(3));
    steady.run(350);
    // From here one network runs A on long frames, the other on short ones.
    for frame in 0..120 {
        if frame % 4 == 0 {
            net.app_mut(A)
                .insert_resource(TimeUpdateStrategy::ManualDuration(TICK * 3));
        } else {
            net.app_mut(A)
                .insert_resource(TimeUpdateStrategy::ManualDuration(TICK));
        }
        net.step();
        steady.step();
    }
    let a = log(&net, HOST_PEER);
    let b = log(&steady, HOST_PEER);
    let shared: Vec<u64> = a
        .samples
        .iter()
        .map(|(tick, _)| *tick)
        .filter(|tick| b.at(*tick).is_some())
        .collect();
    assert!(
        shared.len() > 100,
        "the two sessions share few ticks: {}",
        shared.len()
    );
    for tick in shared {
        assert_eq!(
            a.at(tick),
            b.at(tick),
            "tick {tick} differs between the two sessions"
        );
    }
}
