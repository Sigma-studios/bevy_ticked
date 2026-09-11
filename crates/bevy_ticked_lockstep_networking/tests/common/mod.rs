//! The peers the T6 tests run: a host and clients over `bevy_ensemble_loopback`, simulating a
//! world that is one number.
//!
//! One number on purpose. What these tests are about is the join, the roster and the trust
//! rules, not anybody's physics, and a world that is a counter can be told apart from another
//! with one comparison and pushed out of agreement with one line. Every action is a `u8`, and
//! the tick adds `1 + the sum of that tick's actions` to the counter — so an action that was
//! applied on one peer and not another shows up in the hash, which is the point of having one.

#![allow(dead_code)]

use std::time::Duration;

use bevy::prelude::*;
use bevy::time::TimeUpdateStrategy;
use bevy_ensemble::{EnsemblePlugin, LocalMultiplayerPlayerId, PlayerUUID};
use bevy_ensemble_loopback::{LoopbackNetwork, LoopbackTransportPlugin, PeerId};
use bevy_ticked::prelude::{CurrentTick, TickSource, TickedPlugin, TickedSimulation};
use bevy_ticked_lockstep_networking::{
    ActionTracker, AdaptiveTickBufferPlugin, ApplyJoinSnapshot, CaptureJoinSnapshot,
    ChecksumExchangePlugin, ChecksumLog, ChecksumLogPlugin, Divergence, JoinSnapshotApplied,
    LocalPendingActions, LockstepConfig, LockstepJoinSet, LockstepPlugin, ProvideJoinSnapshot,
    WorldHash,
};
use serde::{Deserialize, Serialize};

/// One tick of virtual time, so one frame of the network is one tick of the simulation.
pub const TICK: Duration = Duration::from_micros(15_625);

pub const HOST: u128 = 1;

/// What a tick adds to the counter, on top of the one it always adds.
pub type Action = u8;
/// The whole world, as sent to a joining client.
pub type Snapshot = u64;

/// The entire simulated world.
#[derive(Resource, Default, Clone, Copy)]
pub struct Counter(pub u64);

/// How many join snapshots this peer has captured, when it is the host.
#[derive(Resource, Default, Clone, Copy)]
pub struct SnapshotsCaptured(pub u32);

#[derive(Clone, Copy, PartialEq, Debug, Serialize, Deserialize)]
pub struct CounterHash {
    pub counter: u64,
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

/// The simulation: one tick, one increment, plus every action anybody scheduled for it.
fn advance_counter(
    mut counter: ResMut<Counter>,
    current_tick: Res<CurrentTick>,
    tracker: Res<ActionTracker<Action>>,
) {
    let actions: u64 = tracker
        .actions_for_tick(current_tick.0)
        .map(|players| {
            players
                .values()
                .flatten()
                .map(|action| u64::from(*action))
                .sum()
        })
        .unwrap_or(0);
    counter.0 += 1 + actions;
}

fn capture_join_snapshot(
    mut requests: MessageReader<CaptureJoinSnapshot<Snapshot>>,
    counter: Res<Counter>,
    mut captured: ResMut<SnapshotsCaptured>,
    mut responses: MessageWriter<ProvideJoinSnapshot<Snapshot>>,
) {
    for request in requests.read() {
        captured.0 += 1;
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

/// How a peer is built, where the defaults are not the only sensible choice.
#[derive(Clone, Copy, Debug)]
pub struct Recipe {
    pub config: LockstepConfig,
    /// Apply join snapshots. A client built without this connects, is verified by the host,
    /// and never loads — a connected peer that is not a participant, which is exactly the
    /// sender the trust rules are about.
    pub finishes_join: bool,
    /// Add `AdaptiveTickBufferPlugin`, so the link sizes the buffer.
    pub adaptive: bool,
    /// Sample the world hash while in no lobby, as a peer idling in a menu would if it used the
    /// core sampler.
    pub sample_without_lobby: bool,
}

impl Default for Recipe {
    fn default() -> Self {
        Self {
            config: LockstepConfig {
                host_tick_buffer: 6,
                client_tick_buffer: 6,
                ..default()
            },
            finishes_join: true,
            adaptive: false,
            sample_without_lobby: false,
        }
    }
}

impl Recipe {
    pub fn with_config(config: LockstepConfig) -> Self {
        Self {
            config,
            ..default()
        }
    }
}

/// A headless peer that behaves like a shipped one.
pub fn peer_with(uuid: PlayerUUID, recipe: Recipe) -> App {
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
                config: recipe.config,
                ..default()
            },
            ChecksumExchangePlugin::<CounterHash>::default(),
        ))
        .insert_resource(LocalMultiplayerPlayerId(uuid))
        .init_resource::<Counter>()
        .init_resource::<SnapshotsCaptured>()
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
            capture_join_snapshot.in_set(LockstepJoinSet::CaptureJoinSnapshot),
        );
    let log = ChecksumLogPlugin::<CounterHash>::default();
    let log = if recipe.sample_without_lobby {
        log.sample_without_lobby()
    } else {
        log
    };
    app.add_plugins(log);
    if recipe.finishes_join {
        app.add_systems(
            Update,
            apply_join_snapshot.in_set(LockstepJoinSet::ApplyJoinSnapshot),
        );
    }
    if recipe.adaptive {
        app.add_plugins(AdaptiveTickBufferPlugin);
    }
    app
}

pub fn peer(uuid: PlayerUUID) -> App {
    peer_with(uuid, Recipe::default())
}

/// A host and one joined client, both simulating, on a perfect link.
pub fn joined_pair() -> LoopbackNetwork {
    let mut net = LoopbackNetwork::new(TICK);
    net.add_host(HOST, peer(HOST));
    net.add_client(2, peer(2));
    // Long enough for the join handshake to finish and for both peers to be running and sampling.
    net.run(200);
    net
}

pub fn tick(net: &LoopbackNetwork, peer: PeerId) -> u64 {
    net.app(peer).world().resource::<CurrentTick>().0
}

pub fn counter(net: &LoopbackNetwork, peer: PeerId) -> u64 {
    net.app(peer).world().resource::<Counter>().0
}

pub fn log(net: &LoopbackNetwork, peer: PeerId) -> &ChecksumLog<CounterHash> {
    net.app(peer).world().resource::<ChecksumLog<CounterHash>>()
}

/// Whether both peers' logs hold a sample for at least `min_shared` of the same ticks, so that
/// "no divergence" between them is a finding and not an absence.
pub fn logs_overlap(net: &LoopbackNetwork, a: PeerId, b: PeerId, min_shared: usize) -> bool {
    let right = log(net, b);
    log(net, a)
        .samples
        .iter()
        .filter(|(tick, _)| right.at(*tick).is_some())
        .count()
        >= min_shared
}

pub fn first_divergence(
    net: &LoopbackNetwork,
    a: PeerId,
    b: PeerId,
) -> Option<Divergence<CounterHash>> {
    log(net, a).first_divergence(log(net, b))
}

/// Step until `condition` holds, and say so if it never did.
pub fn run_until(
    net: &mut LoopbackNetwork,
    max_frames: usize,
    what: &str,
    condition: impl Fn(&LoopbackNetwork) -> bool,
) {
    assert!(
        net.run_until(max_frames, condition),
        "{what} did not happen within {max_frames} frames"
    );
}

/// Step until `peer` has simulated `target`, and say so if it never did.
pub fn run_until_tick(net: &mut LoopbackNetwork, peer: PeerId, target: u64, max_frames: usize) {
    let reached = net.run_until(max_frames, |net| tick(net, peer) >= target);
    assert!(
        reached,
        "peer {peer:?} is at tick {} and never reached {target} in {max_frames} frames",
        tick(net, peer)
    );
}
