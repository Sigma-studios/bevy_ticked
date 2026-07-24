use std::marker::PhantomData;

use bevy::prelude::*;

use bevy_ticked::{
    TickedSet, TickedSimulation,
    registry::TickedComponentRegistry,
    tick::{CurrentTick, TicksPaused},
    tracked_entity::TickTrackedEntityCounter,
};

use crate::{
    input::{InputQueue, TickedInput},
    messages::{ReceivedNetworkSnapshot, SendNetworkInput},
    snapshot::apply_snapshot,
};

/// Resource identifying the local player on the client.
#[derive(Resource)]
pub struct LocalClientPlayer(pub u128);

/// Resource holding a pending server snapshot that needs to be applied.
#[derive(Resource)]
struct PendingSnapshot {
    snapshot: crate::snapshot::WorldSnapshot,
}

/// How many ticks ahead of the server the client runs (its prediction lead).
///
/// The client must lead the server by enough that its inputs arrive before the
/// server reaches the tick they're for. Since the latest snapshot is already
/// one-way-latency stale *and* the client must sit one-way ahead of the server,
/// the required lead is ~one full round-trip (plus a jitter margin) — so this
/// should scale with the peer's RTT rather than be a fixed constant.
///
/// The client uses this both for the initial jump-ahead and as the steady-state
/// target it re-converges toward (one tick per snapshot), so it can be updated
/// live (e.g. from measured RTT) without causing visible tick jumps. Update it
/// via [`ClientTickBuffer::set_from_rtt`], or set `target_ticks` directly.
#[derive(Resource, Clone, Copy, Debug)]
pub struct ClientTickBuffer {
    /// Target lead, in ticks, of the client over the server.
    pub target_ticks: u64,
}

impl Default for ClientTickBuffer {
    fn default() -> Self {
        // Safe fallback until a measured RTT is available (~covers up to ~60ms RTT).
        Self { target_ticks: 6 }
    }
}

impl ClientTickBuffer {
    /// Extra ticks of lead beyond the raw RTT, to absorb network jitter and
    /// once-per-frame delivery/scheduling.
    pub const JITTER_MARGIN_TICKS: u64 = 2;
    /// Never lead by less than this (avoids thrashing on tiny/zero RTT samples).
    pub const MIN_TICKS: u64 = 2;
    /// Cap the lead so a pathological RTT can't make prediction explode.
    pub const MAX_TICKS: u64 = 32;

    /// Size the target lead from a measured round-trip time (seconds).
    ///
    /// The lead needs to be about one RTT (see the type docs) plus a margin.
    pub fn set_from_rtt(&mut self, rtt_seconds: f64) {
        let rtt_ticks = (rtt_seconds.max(0.0)
            / bevy_ticked::tick::SECONDS_PER_TICK as f64)
            .ceil() as u64;
        self.target_ticks =
            (rtt_ticks + Self::JITTER_MARGIN_TICKS).clamp(Self::MIN_TICKS, Self::MAX_TICKS);
    }
}

/// Plugin for the client side of multiplayer tick networking.
///
/// Hooks into `TickedPlugin`'s tick lifecycle:
/// - **PreTick**: if a server snapshot arrived, performs rollback and replay
/// - **PostTick**: sends the local player's input to the server
///
/// The user must provide:
/// - A system in `TickedSimulation` that reads `InputQueue<T>` + `LocalClientPlayer`
///   and applies the local player's input
/// - A system that writes the local player's input into `InputQueue<T>` each tick
pub struct TickedClientPlugin<T: TickedInput> {
    _phantom: PhantomData<T>,
}

impl<T: TickedInput> TickedClientPlugin<T> {
    pub fn new() -> Self {
        Self {
            _phantom: PhantomData,
        }
    }
}

impl<T: TickedInput> Default for TickedClientPlugin<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: TickedInput> Plugin for TickedClientPlugin<T> {
    fn build(&self, app: &mut App) {
        app.init_resource::<InputQueue<T>>()
            .init_resource::<ClientTickBuffer>()
            .add_observer(receive_snapshot)
            .add_systems(
                Update,
                reset_on_join::<T>.run_if(resource_added::<LocalClientPlayer>),
            )
            .add_systems(
                FixedUpdate,
                (
                    handle_server_snapshot::<T>.in_set(TickedSet::PreTick),
                    send_local_input::<T>.in_set(TickedSet::PostTick),
                ),
            );
    }
}

/// Observer: store incoming server snapshot for processing before the next tick.
fn receive_snapshot(trigger: On<ReceivedNetworkSnapshot>, mut commands: Commands) {
    commands.insert_resource(PendingSnapshot {
        snapshot: trigger.event().0.clone(),
    });
}

/// When `LocalClientPlayer` is inserted, reset tick state and pause
/// until the first server snapshot arrives.
fn reset_on_join<T: TickedInput>(world: &mut World) {
    world.insert_resource(CurrentTick(0));
    world.insert_resource(TicksPaused);
    world.insert_resource(TickTrackedEntityCounter::default());
    world.resource_mut::<InputQueue<T>>().inputs.clear();
    let registry = world.resource::<TickedComponentRegistry>().clone();
    registry.clear_all(world);
}

/// PreTick: if a server snapshot arrived, rollback and replay local inputs to now.
fn handle_server_snapshot<T: TickedInput>(world: &mut World) {
    let Some(pending) = world.remove_resource::<PendingSnapshot>() else {
        return;
    };

    let was_paused = world.get_resource::<TicksPaused>().is_some();
    let current_tick = world.resource::<CurrentTick>().0;
    let snapshot_tick = pending.snapshot.tick;
    let tick_buffer = world.resource::<ClientTickBuffer>().target_ticks;

    let registry = world.resource::<TickedComponentRegistry>().clone();

    // Apply the authoritative snapshot (sets CurrentTick to snapshot_tick)
    apply_snapshot(world, &pending.snapshot);

    if snapshot_tick >= current_tick {
        // Snapshot is at or ahead of us — jump forward.
        registry.capture_all(world, snapshot_tick);

        // On initial sync, skip ahead by tick_buffer so our inputs
        // arrive at the server before it reaches those ticks.
        if was_paused {
            let target_tick = snapshot_tick + tick_buffer;
            for tick in (snapshot_tick + 1)..=target_tick {
                world.resource_mut::<CurrentTick>().0 = tick;
                world.run_schedule(TickedSimulation);
                registry.capture_all(world, tick);
            }
            world.remove_resource::<TicksPaused>();
        }
        return;
    }

    // If paused (shouldn't normally happen after initial sync), don't replay
    if was_paused {
        registry.capture_all(world, snapshot_tick);
        world.remove_resource::<TicksPaused>();
        return;
    }

    // Snapshot is behind us — rollback and replay predicted ticks.
    registry.truncate_all_after(world, snapshot_tick);

    // Adaptive lead maintenance: nudge the replay end (and thus the lead) one tick
    // toward `target_ticks` so the client re-converges as RTT changes, without a
    // visible jump. A deadband of [target, target+1] avoids thrashing on ±1 snapshot
    // jitter while never dropping below the target (which would risk late inputs).
    let lead = current_tick - snapshot_tick;
    let end_tick = if lead > tick_buffer + 1 {
        current_tick - 1 // too far ahead: drop one predicted tick
    } else if lead < tick_buffer {
        current_tick + 1 // not far enough ahead: predict one extra tick
    } else {
        current_tick
    };

    for tick in (snapshot_tick + 1)..=end_tick {
        world.resource_mut::<CurrentTick>().0 = tick;
        world.run_schedule(TickedSimulation);
        registry.capture_all(world, tick);
    }
    world.resource_mut::<CurrentTick>().0 = end_tick;
}

/// PostTick: send the local player's input for the current tick to the server.
fn send_local_input<T: TickedInput>(
    tick: Res<CurrentTick>,
    ticks_paused: Option<Res<TicksPaused>>,
    local_player: Option<Res<LocalClientPlayer>>,
    queue: Res<InputQueue<T>>,
    mut commands: Commands,
) {
    if ticks_paused.is_some() {
        return;
    }
    let Some(local_player) = local_player else {
        return;
    };
    let Some(input) = queue.get(tick.0, local_player.0).cloned() else {
        return;
    };
    commands.trigger(SendNetworkInput {
        tick: tick.0,
        input,
    });
}
