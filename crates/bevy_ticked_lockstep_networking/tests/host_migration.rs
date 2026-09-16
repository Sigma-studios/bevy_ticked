//! A lockstep match that outlives its host.
//!
//! The arbiter is the test: `lose_host` is the host becoming unreachable, `name_host` the decision
//! about who replaces it. Everything between — who holds which rulings, where the session resumes,
//! who leaves on which tick — is the crate's, and every assertion that matters ends in the same
//! place: the survivors' worlds agree, tick for tick, from before the host left to long after.

mod common;

use std::time::Duration;

use bevy::prelude::*;
use bevy_ensemble::{HostMigratable, PeerTimeout, encode_ensemble_message};
use bevy_ensemble_loopback::{HostDeparture, Link, LoopbackNetwork, PeerId};
use bevy_ticked::prelude::{TickHoldReason, TickHolds, TickRateDilation, TickedEvents};
use bevy_ticked_lockstep_networking::testing::{migration_state, newest_ruled_tick, push_action};
use bevy_ticked_lockstep_networking::{
    ChecksumReport, Desync, HostMigrationPolicy, LastMigration, LockstepMigration,
    LockstepPauseReason, LockstepPaused, LockstepResumed, LockstepRoster, PauseLockstep,
    ResumeLockstep, ResumeVerdict, RosterChange, StallPolicy,
};
use common::*;

const HOST_PEER: PeerId = PeerId(0);
const A: PeerId = PeerId(1);
const B: PeerId = PeerId(2);
const C: PeerId = PeerId(3);

const MIGRATION: HostMigratable = HostMigratable {
    successor_within: Duration::from_secs(4),
    reach_within: Duration::from_secs(4),
};

/// A host and `clients` joined clients, in a lobby that migrates, running.
fn session(clients: u128) -> LoopbackNetwork {
    session_with(clients, |_| {})
}

fn session_with(
    clients: u128,
    before_running: impl FnOnce(&mut LoopbackNetwork),
) -> LoopbackNetwork {
    let mut net = LoopbackNetwork::new(TICK);
    net.add_host(HOST, peer(HOST));
    for uuid in 2..2 + clients {
        net.add_client(uuid, peer(uuid));
    }
    net.set_host_migration(Some(MIGRATION));
    before_running(&mut net);
    net.run(300);
    for index in 0..=clients as usize {
        assert!(
            tick(&net, PeerId(index)) > 60,
            "peer {index} is not running"
        );
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

/// The tick a peer saw `uuid` leave on, from its roster events.
fn left_at(net: &LoopbackNetwork, p: PeerId, uuid: u128) -> Option<u64> {
    let events = net.app(p).world().resource::<TickedEvents<RosterChange>>();
    (0..=tick(net, p)).find(|t| events.at_tick(*t).contains(&RosterChange::Left(uuid)))
}

fn resume_after(net: &LoopbackNetwork, p: PeerId) -> u64 {
    net.app(p)
        .world()
        .get_resource::<LastMigration>()
        .expect("this peer took part in a migration")
        .resume_after
}

fn holds(net: &LoopbackNetwork, p: PeerId, reason: TickHoldReason) -> bool {
    net.app(p).world().resource::<TickHolds>().holds(reason)
}

fn resumed(net: &LoopbackNetwork, p: PeerId) -> bool {
    migration_state(net.app(p)) == LockstepMigration::Idle
        && net.app(p).world().contains_resource::<LastMigration>()
}

/// Worlds that agree on every tick both sampled, and share enough of them for that to mean
/// something.
fn assert_agree(net: &LoopbackNetwork, a: PeerId, b: PeerId) {
    assert!(
        logs_overlap(net, a, b, 50),
        "peers {a:?} and {b:?} sampled too few of the same ticks to compare"
    );
    assert_eq!(first_divergence(net, a, b), None, "peers {a:?} and {b:?}");
}

/// A link on which the host's rulings reach `to` `ticks` ticks late, so `to` holds fewer of them
/// than everyone else the moment the host goes.
fn lagging_rulings(net: &mut LoopbackNetwork, to: PeerId, ticks: u32) {
    net.set_link_between(HOST_PEER, to, Link::delayed(TICK * ticks));
}

// ── waiting ──────────────────────────────────────────────────────────────────

#[test]
fn every_peer_holds_its_clock_while_it_waits_for_a_new_host() {
    let mut net = session(2);
    net.lose_host(HostDeparture::Crashes);
    net.run(10);
    let before = (tick(&net, A), tick(&net, B));
    net.run(30);
    for p in [A, B] {
        assert!(holds(&net, p, TickHoldReason::HostMigration), "peer {p:?}");
    }
    assert_eq!((tick(&net, A), tick(&net, B)), before);
}

#[test]
fn a_host_that_is_never_replaced_ends_the_session_and_releases_every_hold() {
    let mut net = session(2);
    net.set_host_migration(Some(HostMigratable {
        successor_within: Duration::from_millis(300),
        ..MIGRATION
    }));
    net.lose_host(HostDeparture::Crashes);
    net.run(60);
    for p in [A, B] {
        assert!(
            net.try_lobby(p).is_none() || !net.app(p).world().contains_resource::<LastMigration>()
        );
        let holds = net.app(p).world().resource::<TickHolds>();
        assert!(
            !holds.holds(TickHoldReason::HostMigration)
                && !holds.holds(TickHoldReason::WaitingForPeers),
            "peer {p:?} is still held: {holds:?}"
        );
    }
}

/// A host whose process froze — a debugger, a backgrounded tab — is silent without anything
/// having been lost: what it sends when it wakes is delivered. The survivors wait for a successor
/// as they would for a host that is gone, and take this one back when it answers.
#[test]
fn a_host_that_came_back_before_a_successor_was_named_resumes_without_a_migration() {
    let mut net = session(2);
    for p in [A, B] {
        net.app_mut(p)
            .insert_resource(PeerTimeout(Some(Duration::from_millis(200))));
    }
    for _ in 0..40 {
        net.step_only(&[A, B]);
    }
    assert!(holds(&net, A, TickHoldReason::HostMigration));

    let before = tick(&net, A);
    run_until(&mut net, 400, "the session to run again", |net| {
        tick(net, A) > before + 100 && tick(net, B) > before + 100
    });
    for p in [A, B] {
        assert!(!net.app(p).world().contains_resource::<LastMigration>());
        assert_agree(&net, HOST_PEER, p);
    }
}

// ── resuming ─────────────────────────────────────────────────────────────────

#[test]
fn a_host_that_leaves_hands_the_match_to_a_survivor_who_resumes_it_in_agreement() {
    let mut net = session(2);
    let before = tick(&net, A);
    net.migrate(A);
    run_until(&mut net, 600, "the survivors to resume and run on", |net| {
        resumed(net, A) && tick(net, A) > before + 150 && tick(net, B) > before + 150
    });
    assert_agree(&net, A, B);
    assert_eq!(roster(&net, A), [2, 3]);
    assert_eq!(roster(&net, B), [2, 3]);
}

#[test]
fn the_departed_host_leaves_the_roster_on_the_first_tick_the_new_host_rules() {
    let mut net = session(2);
    net.migrate(A);
    run_until(&mut net, 600, "the survivors to resume", |net| {
        resumed(net, A) && resumed(net, B)
    });
    net.run(60);
    let first_ruled = resume_after(&net, A) + 1;
    assert_eq!(resume_after(&net, B), first_ruled - 1);
    assert_eq!(left_at(&net, A, HOST), Some(first_ruled));
    assert_eq!(left_at(&net, B, HOST), Some(first_ruled));
}

#[test]
fn survivors_that_held_different_last_ticks_are_filled_in_to_the_furthest() {
    let mut net = session_with(2, |net| lagging_rulings(net, B, 12));
    net.lose_host(HostDeparture::Crashes);
    let (furthest, behind) = (
        newest_ruled_tick::<Action>(net.app(A)).unwrap(),
        newest_ruled_tick::<Action>(net.app(B)).unwrap(),
    );
    assert!(
        furthest > behind,
        "A holds more of the old host's rulings than B"
    );

    net.name_host(A);
    run_until(&mut net, 800, "the survivors to resume and run on", |net| {
        resumed(net, B) && tick(net, B) > furthest + 100
    });
    assert_eq!(resume_after(&net, A), furthest);
    assert_agree(&net, A, B);
}

#[test]
fn a_new_host_that_is_the_most_behind_takes_the_missing_ticks_from_a_survivor() {
    let mut net = session_with(2, |net| lagging_rulings(net, B, 12));
    net.lose_host(HostDeparture::Crashes);
    let furthest = newest_ruled_tick::<Action>(net.app(A)).unwrap();
    assert!(furthest > newest_ruled_tick::<Action>(net.app(B)).unwrap());

    net.name_host(B);
    run_until(&mut net, 800, "the survivors to resume and run on", |net| {
        resumed(net, B) && tick(net, A) > furthest + 100
    });
    assert_eq!(
        resume_after(&net, B),
        furthest,
        "the new host resumed from the furthest survivor's rulings, not its own"
    );
    assert_agree(&net, A, B);
}

#[test]
fn an_action_scheduled_before_the_host_left_and_never_ruled_is_ruled_by_the_new_host() {
    let mut net = session(2);
    for p in [A, B] {
        assert_eq!(
            counter(&net, p),
            tick(&net, p),
            "no action has been taken yet"
        );
    }
    push_action(net.app_mut(B), 5u8);
    // Sent to the host, which never reads it.
    net.run(1);
    net.lose_host(HostDeparture::Crashes);
    net.name_host(A);
    run_until(&mut net, 600, "the survivors to resume and run on", |net| {
        resumed(net, B) && tick(net, A) > resume_after(net, A) + 60
    });
    net.run(30);
    for p in [A, B] {
        assert_eq!(
            counter(&net, p) - tick(&net, p),
            5,
            "peer {p:?} applied the action exactly once"
        );
    }
    assert_agree(&net, A, B);
}

#[test]
fn a_survivor_that_never_reports_leaves_on_the_same_tick_everywhere() {
    let mut net = session(3);
    net.set_host_migration(Some(HostMigratable {
        reach_within: Duration::from_millis(500),
        ..MIGRATION
    }));
    net.lose_host(HostDeparture::Crashes);
    net.disconnect(C);
    net.name_host(A);
    run_until(&mut net, 800, "the survivors to resume without C", |net| {
        resumed(net, A) && resumed(net, B)
    });
    net.run(60);
    let first_ruled = resume_after(&net, A) + 1;
    assert_eq!(left_at(&net, A, 4), Some(first_ruled));
    assert_eq!(left_at(&net, B, 4), Some(first_ruled));
    assert_eq!(roster(&net, A), [2, 3]);
    assert_agree(&net, A, B);
}

#[test]
fn a_joiner_caught_mid_join_by_a_host_loss_joins_the_new_host_from_a_fresh_snapshot() {
    let mut net = session(2);
    let joiner = net.add_client(4, peer(4));
    net.run(3);
    net.migrate(A);
    run_until(
        &mut net,
        1200,
        "the joiner to be simulating with the new host",
        |net| roster(net, A).contains(&4) && tick(net, joiner) > resume_after(net, A) + 150,
    );
    assert!(net.app(A).world().resource::<SnapshotsCaptured>().0 >= 1);
    assert_agree(&net, A, joiner);
    assert_agree(&net, A, B);
}

#[test]
fn a_second_host_loss_during_the_resume_is_resumed_from_again() {
    let mut net = session(3);
    net.lose_host(HostDeparture::Crashes);
    net.name_host(A);
    net.run(1);
    net.lose_host(HostDeparture::Crashes);
    net.name_host(B);
    run_until(&mut net, 800, "B and C to resume", |net| {
        resumed(net, B) && resumed(net, C)
    });
    net.run(120);
    let first_ruled = resume_after(&net, B) + 1;
    for uuid in [HOST, 2] {
        assert_eq!(left_at(&net, B, uuid), Some(first_ruled), "{uuid} on B");
        assert_eq!(left_at(&net, C, uuid), Some(first_ruled), "{uuid} on C");
    }
    assert_eq!(roster(&net, C), [3, 4]);
    assert_agree(&net, B, C);
}

#[test]
fn a_survivor_holding_ticks_past_the_trust_window_rejoins_rather_than_being_trusted() {
    let mut net = session_with(2, |net| {
        lagging_rulings(net, B, 30);
        for p in [HOST_PEER, A, B] {
            net.app_mut(p).insert_resource(HostMigrationPolicy {
                trust_window: 8,
                ..default()
            });
        }
    });
    net.lose_host(HostDeparture::Crashes);
    let (furthest, own) = (
        newest_ruled_tick::<Action>(net.app(A)).unwrap(),
        newest_ruled_tick::<Action>(net.app(B)).unwrap(),
    );
    assert!(
        furthest > own + 8,
        "A holds more than a trust window beyond B"
    );

    net.name_host(B);
    run_until(&mut net, 2000, "A to have left and joined again", |net| {
        net.app(B).world().contains_resource::<LastMigration>()
            && left_at(net, B, 2).is_some()
            && roster(net, B).contains(&2)
            && tick(net, A) > resume_after(net, B) + 150
    });
    let resumed_at = resume_after(&net, B);
    assert!(
        resumed_at <= own + 8 && resumed_at < furthest,
        "the new host took nothing past its window: resumed at {resumed_at}, its own rulings \
         end at {own}, A's at {furthest}"
    );
    assert_eq!(
        left_at(&net, B, 2),
        Some(resumed_at + 1),
        "A, too far ahead to resume, left on the first tick the new host ruled"
    );
    net.run(100);
    assert_agree(&net, B, A);
}

#[test]
fn a_parked_checksum_report_for_a_tick_the_new_host_re_rules_is_forgotten() {
    let mut net = session(2);
    let future = tick(&net, B) + 60;
    let bytes = {
        let registry = net
            .app(B)
            .world()
            .resource::<bevy_ensemble::EnsembleMessageRegistry>();
        encode_ensemble_message(
            registry,
            &ChecksumReport {
                tick: future,
                hash: CounterHash { counter: u64::MAX },
            },
        )
    };
    net.deliver_raw(B, HOST, bytes);
    net.run(1);
    net.migrate(A);
    run_until(&mut net, 800, "B to run past the reported tick", |net| {
        tick(net, B) > future + 10
    });
    assert!(
        !net.app(B)
            .world()
            .contains_resource::<Desync<CounterHash>>(),
        "a report the old host made about a tick the new host ruled again is not a desync"
    );
}

#[test]
fn a_session_paused_when_the_host_left_stays_paused_until_the_new_host_resumes_it() {
    let mut net = session(2);
    net.app_mut(HOST_PEER)
        .world_mut()
        .write_message(PauseLockstep(LockstepPauseReason::Host));
    net.run(40);
    net.migrate(A);
    run_until(
        &mut net,
        600,
        "the survivors to agree where to resume",
        |net| {
            net.app(A).world().contains_resource::<LastMigration>()
                && net.app(B).world().contains_resource::<LastMigration>()
        },
    );
    net.run(60);
    let paused_at = (tick(&net, A), tick(&net, B));
    net.run(60);
    assert_eq!((tick(&net, A), tick(&net, B)), paused_at, "still paused");
    for p in [A, B] {
        assert!(net.app(p).world().resource::<LockstepPaused>().0.is_some());
    }

    net.app_mut(A).world_mut().write_message(ResumeLockstep);
    run_until(&mut net, 400, "the session to run again", |net| {
        tick(net, A) > paused_at.0 + 60 && tick(net, B) > paused_at.1 + 60
    });
    assert_agree(&net, A, B);
}

#[test]
fn the_stall_timer_does_not_kick_a_survivor_while_reports_are_collected() {
    let mut net = session(2);
    net.app_mut(A).insert_resource(StallPolicy {
        pause_after: Duration::from_millis(50),
        kick_after: Some(Duration::from_millis(150)),
    });
    net.lose_host(HostDeparture::Crashes);
    net.name_host(A);
    for _ in 0..30 {
        net.step_only(&[A]);
    }
    run_until(&mut net, 600, "the survivors to resume", |net| {
        resumed(net, A) && resumed(net, B)
    });
    net.run(100);
    assert!(net.try_lobby(B).is_some(), "B was not kicked");
    assert_eq!(roster(&net, A), [2, 3]);
    assert_agree(&net, A, B);
}

#[test]
fn a_promoted_client_stops_dilating_once_it_hosts() {
    let mut net = session(2);
    net.migrate(A);
    run_until(&mut net, 600, "the survivors to resume", |net| {
        resumed(net, A) && resumed(net, B)
    });
    net.run(100);
    assert_eq!(net.app(A).world().resource::<TickRateDilation>().0, 1.0);
}

/// A game reads where the session resumed, and whether this peer is in it, from one message.
#[test]
fn every_survivor_is_told_where_the_session_resumed() {
    #[derive(Resource, Default)]
    struct Told(Vec<LockstepResumed>);
    let mut net = session(2);
    for p in [A, B] {
        net.app_mut(p).init_resource::<Told>().add_systems(
            Update,
            |mut resumed: MessageReader<LockstepResumed>, mut told: ResMut<Told>| {
                told.0.extend(resumed.read().copied());
            },
        );
    }
    net.migrate(A);
    run_until(&mut net, 600, "the survivors to resume", |net| {
        resumed(net, A) && resumed(net, B)
    });
    let told = |p: PeerId| net.app(p).world().resource::<Told>().0.clone();
    assert_eq!(told(A), told(B));
    assert_eq!(
        told(A),
        [LockstepResumed {
            previous_host: HOST,
            resume_after: resume_after(&net, A),
            verdict: ResumeVerdict::Continue,
        }]
    );
}
