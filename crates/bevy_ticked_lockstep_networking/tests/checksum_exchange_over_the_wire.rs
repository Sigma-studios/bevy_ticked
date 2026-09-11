//! Two peers, a real transport, and a divergence that has to reach somebody.
//!
//! [`checksum_exchange`](bevy_ticked_lockstep_networking::checksum_exchange)'s own tests inject
//! [`ChecksumReport`]s straight into the message queue, which exercises the comparison and
//! nothing else. That is not enough here, and the reason is the bug this module exists for: the
//! previous state of the art was a divergence search that was correct, tested, and wired to
//! nothing. A unit test would have passed then too.
//!
//! So these run the whole path — sample, announce, serialise, cross a simulated link, arrive,
//! park while the client catches up, compare — over `bevy_ensemble_loopback`, on two peers that
//! join the way real ones do.
//!
//! # The simulated world is one number
//!
//! Deliberately. What is being tested is the reporting, not anybody's simulation, and a world
//! that is a single counter can be pushed out of agreement on one peer with one line — which is
//! the whole experiment. A divergence in a real game is only ever this with more steps.

use std::time::Duration;

use bevy::prelude::*;
use bevy::time::TimeUpdateStrategy;
use bevy_ensemble::{EnsemblePlugin, LocalMultiplayerPlayerId, PlayerUUID};
use bevy_ensemble_loopback::{LoopbackNetwork, LoopbackTransportPlugin};
use bevy_ticked::prelude::{
    CurrentTick, TickHoldReason, TickHolds, TickSource, TickedPlugin, TickedSimulation,
};
use bevy_ticked_lockstep_networking::{
    ActionTracker, ApplyJoinSnapshot, CaptureJoinSnapshot, ChecksumExchangePlugin, ChecksumLog,
    ChecksumLogPlugin, Desync, JoinSnapshotApplied, LocalPendingActions, LockstepConfig,
    LockstepJoinSet, LockstepPlugin, ProvideJoinSnapshot, WorldHash,
};
use serde::{Deserialize, Serialize};

/// One tick of virtual time, so one frame of the network is one tick of the simulation.
const TICK: Duration = Duration::from_micros(15_625);

/// Nothing anybody does; the simulation advances on its own. A desync needs no input to happen.
type Action = u8;
/// The whole world, as sent to a joining client.
type Snapshot = u64;

/// The entire simulated world.
#[derive(Resource, Default, Clone, Copy)]
struct Counter(u64);

#[derive(Clone, Copy, PartialEq, Debug, Serialize, Deserialize)]
struct CounterHash {
    counter: u64,
}

impl WorldHash for CounterHash {
    fn sample(world: &mut World) -> Self {
        CounterHash {
            counter: world.resource::<Counter>().0,
        }
    }

    fn value(&self) -> u64 {
        self.counter
    }

    fn differences(&self, other: &Self) -> Vec<&'static str> {
        if self.counter == other.counter {
            Vec::new()
        } else {
            vec!["counter"]
        }
    }
}

/// The simulation: one tick, one increment. Identical on every peer, which is the point.
fn advance_counter(mut counter: ResMut<Counter>) {
    counter.0 += 1;
}

fn capture_join_snapshot(
    mut requests: MessageReader<CaptureJoinSnapshot<Snapshot>>,
    counter: Res<Counter>,
    mut responses: MessageWriter<ProvideJoinSnapshot<Snapshot>>,
) {
    for request in requests.read() {
        responses.write(ProvideJoinSnapshot {
            requester: request.requester,
            snapshot_tick: request.snapshot_tick,
            snapshot: counter.0,
        });
    }
}

fn apply_join_snapshot(
    mut commands: Commands,
    mut snapshots: MessageReader<ApplyJoinSnapshot<Snapshot>>,
    mut counter: ResMut<Counter>,
    mut current_tick: ResMut<CurrentTick>,
    mut tracker: ResMut<ActionTracker<Action>>,
    mut applied: MessageWriter<JoinSnapshotApplied<Snapshot>>,
) {
    for snapshot in snapshots.read() {
        current_tick.0 = snapshot.snapshot_tick;
        counter.0 = snapshot.snapshot;
        tracker.ticks.clear();
        commands.insert_resource(LocalPendingActions::<Action>::default());
        applied.write(JoinSnapshotApplied::new(snapshot.snapshot_tick));
    }
}

fn peer(uuid: PlayerUUID) -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .insert_resource(TimeUpdateStrategy::ManualDuration(TICK))
        .add_plugins((
            TickedPlugin {
                source: TickSource::Hz(64.0),
                ..default()
            },
            EnsemblePlugin,
            LoopbackTransportPlugin,
            LockstepPlugin::<Action, Snapshot> {
                config: LockstepConfig {
                    host_tick_buffer: 6,
                    client_tick_buffer: 6,
                    ..default()
                },
                ..default()
            },
            ChecksumLogPlugin::<CounterHash>::default(),
            ChecksumExchangePlugin::<CounterHash>::default(),
        ))
        .insert_resource(LocalMultiplayerPlayerId(uuid))
        .init_resource::<Counter>()
        // Every tick. Two peers are almost never on the same tick at the same moment — a client
        // trails the host by whatever its join cost — so what gets compared is what each recorded
        // *at tick N*, and that needs both of them to have recorded every N.
        .insert_resource(ChecksumLog::<CounterHash> {
            interval: 1,
            capacity: 4096,
            samples: Vec::new(),
        })
        .add_systems(TickedSimulation, advance_counter)
        .add_systems(
            Update,
            (
                capture_join_snapshot.in_set(LockstepJoinSet::CaptureJoinSnapshot),
                apply_join_snapshot.in_set(LockstepJoinSet::ApplyJoinSnapshot),
            ),
        );
    app
}

/// A host and one joined client, both simulating, on a link with real delay.
fn joined_pair() -> LoopbackNetwork {
    let mut net = LoopbackNetwork::new(TICK);
    net.add_host(1, peer(1));
    net.add_client(2, peer(2));
    // Long enough for the join handshake to finish and for both peers to be running and sampling.
    net.run(200);
    net
}

fn desync_on(net: &LoopbackNetwork, peer: usize) -> Option<Desync<CounterHash>> {
    net.app(bevy_ensemble_loopback::PeerId(peer))
        .world()
        .get_resource::<Desync<CounterHash>>()
        .cloned()
}

fn current_tick(net: &LoopbackNetwork, peer: usize) -> u64 {
    net.app(bevy_ensemble_loopback::PeerId(peer))
        .world()
        .resource::<CurrentTick>()
        .0
}

#[test]
fn two_peers_that_agree_are_never_reported() {
    let mut net = joined_pair();
    net.run(300);

    assert!(
        current_tick(&net, 1) > 100,
        "the client has to actually be simulating, or this asserts nothing at all"
    );
    assert!(
        desync_on(&net, 0).is_none() && desync_on(&net, 1).is_none(),
        "the two peers ran the same simulation, and a checker that cries wolf on that is worse \
         than no checker"
    );
}

#[test]
fn a_client_that_diverges_learns_of_it_and_so_does_the_host() {
    let mut net = joined_pair();

    // The divergence: one peer's world moves and the other's does not. Every hash from here on
    // differs, exactly as a real desync does.
    net.app_mut(bevy_ensemble_loopback::PeerId(1))
        .world_mut()
        .resource_mut::<Counter>()
        .0 += 1_000;
    let poisoned_at = current_tick(&net, 1);

    let reported = net.run_until(400, |net| {
        desync_on(net, 0).is_some() && desync_on(net, 1).is_some()
    });

    assert!(
        reported,
        "the client's world stopped matching the host's and neither peer said so — which is the \
         whole failure this module exists to end"
    );

    let on_client = desync_on(&net, 1).expect("the client compared and disagreed");
    assert_eq!(
        on_client.divergence.sections,
        vec!["counter"],
        "a report that names no section leaves the reader bisecting a u64"
    );
    assert!(
        on_client.divergence.tick >= poisoned_at,
        "reported tick {} is before the world was poisoned at {poisoned_at}",
        on_client.divergence.tick
    );

    let on_host = desync_on(&net, 0).expect("the client told the host, so the host knows too");
    assert_eq!(
        on_host.divergence.tick, on_client.divergence.tick,
        "both peers should be describing the same disagreement about the same tick"
    );
}

#[test]
fn the_report_survives_a_link_that_drops_and_reorders() {
    let mut net = joined_pair();
    // `ChecksumReport` travels unreliably and unordered on purpose — see the module docs — so the
    // one thing that must not happen is that a lossy link quietly turns the checker off.
    net.set_link(bevy_ensemble_loopback::Link::bad_wifi());

    net.app_mut(bevy_ensemble_loopback::PeerId(1))
        .world_mut()
        .resource_mut::<Counter>()
        .0 += 1_000;

    assert!(
        net.run_until(2000, |net| desync_on(net, 1).is_some()),
        "a dropped report costs one interval of detection latency and nothing more; a checker \
         that only works on a perfect link is a checker that never runs where it is needed"
    );
}

// ── The pause vocabulary ─────────────────────────────────────────────────────

/// A lockstep client holds and releases `WaitingForPeers` every frame from what it has
/// received. It used to do that with the one pause marker, so the next authoritative tick to
/// arrive un-paused a game whose player had opened the menu.
#[test]
fn a_user_pause_is_not_lifted_by_an_arriving_authoritative_tick() {
    let mut net = joined_pair();
    let client = bevy_ensemble_loopback::PeerId(1);
    net.app_mut(client)
        .world_mut()
        .resource_mut::<TickHolds>()
        .hold(TickHoldReason::Manual);
    let paused_at = current_tick(&net, 1);

    net.run(100);

    let holds = net.app(client).world().resource::<TickHolds>();
    assert!(
        holds.holds(TickHoldReason::Manual),
        "the menu is still open"
    );
    assert_eq!(
        current_tick(&net, 1),
        paused_at,
        "the lockstep sync ran a hundred frames and none of them moved a paused client"
    );
    // The host waits on the paused client's actions: a stall, until the lockstep phase's
    // stall policy pauses the session and then kicks. Today it simply waits.
    let host_while_paused = current_tick(&net, 0);

    net.app_mut(client)
        .world_mut()
        .resource_mut::<TickHolds>()
        .release(TickHoldReason::Manual);
    net.run(200);
    assert!(
        current_tick(&net, 1) > paused_at + 50,
        "released, the client runs again ({} -> {})",
        paused_at,
        current_tick(&net, 1)
    );
    assert!(
        current_tick(&net, 0) > host_while_paused + 50,
        "and the host with it"
    );
}
