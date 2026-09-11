//! Asking a peer what it thinks, in the vocabulary of the thing under test.
//!
//! Free functions over `&App`, in the style of `bevy_ticked_lockstep_networking::testing`, so
//! they compose with whatever drives the peers — [`TickedNetwork`](crate::net::TickedNetwork), a
//! bare `LoopbackNetwork`, or a game's own wrapper. What was missing was never the loop but the
//! words: every consumer reached into `CurrentTick`, `AppliedSnapshotTick` and `ClientTickBuffer`
//! by hand, and each of them got at least one of the accessors subtly wrong — the lead computed
//! the wrong way round, a `u64` subtraction that panicked when a client fell behind.

use std::collections::BTreeMap;

use bevy::prelude::*;
use bevy_ticked::registry::TickedComponent;
use bevy_ticked::tick::{CurrentTick, TickHolds};
use bevy_ticked::tracked_entity::TickTrackedEntity;
use bevy_ticked::tracked_index::TrackedEntityIndex;
use bevy_ticked::world_actions::WorldActions;
use bevy_ticked_networking::client::{AppliedSnapshotTick, ClientTickBuffer, LocalClientPlayer};
use bevy_ticked_networking::diagnostics::{HealthWarnings, InputStats, ReplayStats, SnapshotStats};
use bevy_ticked_networking::input::{InputQueue, TickedInput};
use bevy_ticked_networking::replication::{AuthoritativeHistory, ReplicationMode};
use bevy_ticked_networking::server::LocalServerPlayer;

pub use crate::net::Role;

#[cfg(feature = "lockstep")]
pub use bevy_ticked_lockstep_networking::testing::*;

/// The tick this peer has simulated up to.
pub fn tick(app: &App) -> u64 {
    app.world().resource::<CurrentTick>().0
}

/// Whether the tick clock is held. On a client that is the normal state between joining and the
/// first snapshot, not a fault.
pub fn paused(app: &App) -> bool {
    app.world().resource::<TickHolds>().is_held()
}

/// How many ticks `client` runs ahead of `host`. Negative when it has fallen behind, which is
/// exactly the case a `u64` subtraction here used to panic on.
pub fn lead(client: &App, host: &App) -> i64 {
    tick(client) as i64 - tick(host) as i64
}

/// Which role this peer currently holds.
pub fn role(app: &App) -> Role {
    let world = app.world();
    if world.contains_resource::<LocalServerPlayer>() {
        Role::Host
    } else if world.contains_resource::<LocalClientPlayer>() {
        Role::Client
    } else {
        Role::Solo
    }
}

/// The tick of the last snapshot this client applied; `None` before the first of a session, or
/// on a peer without the client plugin.
pub fn applied_tick(app: &App) -> Option<u64> {
    app.world()
        .get_resource::<AppliedSnapshotTick>()
        .and_then(|applied| applied.0)
}

/// The newest tick this peer holds an authoritative record for: what its interpolated
/// entities are drawn from, `InterpolationDelay` ticks behind. `None` before the first snapshot
/// of a session, or on a peer without the client plugin.
///
/// The same number as [`applied_tick`] in a running session — the record is kept as the
/// snapshot is applied — but read from `AuthoritativeHistory`, because that is what the
/// restore reads, and a test about what a remote body shows wants the source the plugin uses.
pub fn authoritative_tick(app: &App) -> Option<u64> {
    app.world()
        .get_resource::<AuthoritativeHistory>()?
        .newest_tick()
}

/// How this peer treats the entity carrying `id`: `Some(Predicted)` for the local player's
/// (or anything a game marked so), `None` for everything else — which is interpolated, the
/// default the marker's absence means. `None` also when no entity carries `id`.
pub fn replication_mode(app: &App, id: u64) -> Option<ReplicationMode> {
    let entity = tracked_entity(app, id)?;
    app.world().get::<ReplicationMode>(entity).copied()
}

fn client_buffer(app: &App) -> &ClientTickBuffer {
    app.world()
        .get_resource::<ClientTickBuffer>()
        .expect("TickedClientPlugin is not on this peer, so it has no tick buffer to read")
}

/// What `current_tick - snapshot_tick` is steering toward on this client. Not the lead — see
/// `ClientTickBuffer` for the difference, which is one one-way trip.
pub fn target_replay_distance(app: &App) -> u64 {
    client_buffer(app).target_replay_distance
}

/// How many ticks early this client is trying to land its inputs at the server.
pub fn target_margin(app: &App) -> i64 {
    client_buffer(app).target_margin
}

/// Rollback and replay counters, zero if the client plugin is absent.
pub fn replays(app: &App) -> ReplayStats {
    app.world()
        .get_resource::<ReplayStats>()
        .copied()
        .unwrap_or_default()
}

/// Snapshot broadcast counters, zero if the server plugin is absent.
pub fn snapshot_stats(app: &App) -> SnapshotStats {
    app.world()
        .get_resource::<SnapshotStats>()
        .cloned()
        .unwrap_or_default()
}

/// Input arrival counters, zero if the server plugin is absent.
pub fn input_stats(app: &App) -> InputStats {
    app.world()
        .get_resource::<InputStats>()
        .cloned()
        .unwrap_or_default()
}

/// What the stack has noticed going wrong, zero if nothing has.
pub fn health(app: &App) -> HealthWarnings {
    app.world()
        .get_resource::<HealthWarnings>()
        .cloned()
        .unwrap_or_default()
}

/// The inclusive `(oldest, newest)` tick this peer holds history of `T` for.
pub fn history_range<T: TickedComponent>(app: &App) -> Option<(u64, u64)> {
    app.world()
        .get_resource::<WorldActions<T>>()?
        .recorded_range()
}

/// Every tracked id in this world, ascending, from the entities themselves rather than the
/// index — so a duplicate, which the index cannot represent, is visible.
pub fn tracked_ids(app: &mut App) -> Vec<u64> {
    let world = app.world_mut();
    let mut ids: Vec<u64> = world
        .query::<&TickTrackedEntity>()
        .iter(world)
        .map(|tracked| tracked.0)
        .collect();
    ids.sort_unstable();
    ids
}

/// The entity carrying tracked id `id`, via the index `TickedPlugin` maintains.
pub fn tracked_entity(app: &App, id: u64) -> Option<Entity> {
    app.world().resource::<TrackedEntityIndex>().get(id)
}

/// How many tracked entities are alive (tombstones excluded).
pub fn tracked_entity_count(app: &App) -> usize {
    app.world().resource::<TrackedEntityIndex>().len()
}

/// How many tracked entities are tombstoned: despawned, kept for a rewind.
pub fn tombstone_count(app: &App) -> usize {
    app.world()
        .resource::<TrackedEntityIndex>()
        .tombstones()
        .count()
}

/// What history says `T` was on `id` at `tick`.
pub fn component_at<T: TickedComponent>(app: &App, id: u64, tick: u64) -> Option<T> {
    app.world()
        .get_resource::<WorldActions<T>>()?
        .at_tick(tick)?
        .get(&id)
        .cloned()
}

/// `T` as it stands on the entity carrying `id` right now.
pub fn latest<T: Component + Clone>(app: &App, id: u64) -> Option<T> {
    let entity = tracked_entity(app, id)?;
    app.world().get::<T>(entity).cloned()
}

/// A copy of a peer's input queue, taken at one instant so a test can look at it twice.
#[derive(Clone, Debug)]
pub struct InputQueueView<I> {
    inputs: BTreeMap<u64, BTreeMap<u128, I>>,
}

impl<I: Clone> InputQueueView<I> {
    /// Every tick with at least one input, ascending.
    pub fn ticks(&self) -> Vec<u64> {
        self.inputs.keys().copied().collect()
    }

    /// Every player's input at `tick`.
    pub fn at(&self, tick: u64) -> BTreeMap<u128, I> {
        self.inputs.get(&tick).cloned().unwrap_or_default()
    }

    /// The newest tick this queue holds an input from `uuid` for. `None` is how a test tells
    /// "the input was lost" from "it arrived and did nothing".
    pub fn newest_for(&self, uuid: u128) -> Option<u64> {
        self.inputs
            .iter()
            .rev()
            .find(|(_, players)| players.contains_key(&uuid))
            .map(|(tick, _)| *tick)
    }

    /// How many ticks hold any input.
    pub fn len(&self) -> usize {
        self.inputs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.inputs.is_empty()
    }
}

/// A stable view of what this peer holds in its `InputQueue<I>`.
pub fn input_queue<I: TickedInput>(app: &App) -> InputQueueView<I> {
    let queue = app
        .world()
        .get_resource::<InputQueue<I>>()
        .expect("neither role plugin is on this peer, so it has no input queue");
    InputQueueView {
        inputs: queue.inputs.clone(),
    }
}
