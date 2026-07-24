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
/// server reaches the tick they're for. This sizes itself from the *actual* input
/// timeliness, self-contained in this crate: the server measures how many ticks
/// early/late each client's inputs arrive and reports it in every snapshot (see
/// [`WorldSnapshot::input_margins`](crate::snapshot::WorldSnapshot)); the client
/// then solves directly for the lead that keeps a small positive margin, and
/// re-converges toward it one tick per snapshot (no transport RTT needed, no
/// visible tick jumps). `target_ticks` is exposed for read-only display.
#[derive(Resource, Clone, Copy, Debug)]
pub struct ClientTickBuffer {
    /// Target lead, in ticks, of the client over the server.
    pub target_ticks: u64,
    /// EWMA accumulator for the target, so per-snapshot margin jitter doesn't
    /// make the lead wander.
    smoothed: f64,
}

impl Default for ClientTickBuffer {
    fn default() -> Self {
        // Starting lead until the first margin measurement arrives.
        Self {
            target_ticks: 6,
            smoothed: 6.0,
        }
    }
}

impl ClientTickBuffer {
    /// Desired input-arrival margin: inputs should reach the server this many
    /// ticks early, to absorb jitter and once-per-frame delivery.
    const TARGET_MARGIN: i64 = 2;
    /// Never lead by less than this.
    const MIN_TICKS: u64 = 2;
    /// Cap the lead so a pathological connection can't make prediction explode.
    const MAX_TICKS: u64 = 64;
    /// EWMA weight for new observations.
    const SMOOTHING: f64 = 0.1;

    /// Update the target lead from an observed replay distance
    /// (`current_tick - snapshot_tick`) and the server-measured input margin.
    ///
    /// With `replay_distance = lead + one_way` and `margin = lead - one_way`, the
    /// lead that yields `TARGET_MARGIN` is `replay_distance - margin + TARGET_MARGIN`.
    /// This is a stable fixed point, EWMA-smoothed against jitter.
    fn observe(&mut self, replay_distance: u64, margin: i64) {
        let raw = (replay_distance as i64 - margin + Self::TARGET_MARGIN)
            .clamp(Self::MIN_TICKS as i64, Self::MAX_TICKS as i64) as f64;
        self.smoothed = (1.0 - Self::SMOOTHING) * self.smoothed + Self::SMOOTHING * raw;
        self.target_ticks = (self.smoothed.round() as u64).clamp(Self::MIN_TICKS, Self::MAX_TICKS);
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

    let lead = current_tick - snapshot_tick;

    // Self-adaptive lead: update the target from the server-reported input margin
    // for this client (how early/late its inputs are arriving), self-contained in
    // this crate.
    if let Some(uuid) = world.get_resource::<LocalClientPlayer>().map(|p| p.0) {
        if let Some(&margin) = pending.snapshot.input_margins.get(&uuid) {
            world.resource_mut::<ClientTickBuffer>().observe(lead, margin);
        }
    }
    let target = world.resource::<ClientTickBuffer>().target_ticks;

    // Re-converge one tick toward the target lead (deadband [target, target+1];
    // never below target, which would risk late inputs) — no visible jump.
    let end_tick = if lead > target + 1 {
        current_tick - 1 // too far ahead: drop one predicted tick
    } else if lead < target {
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

/// Number of recent ticks of input included in each packet. Input for tick T
/// also rides in the packets sent at T+1 and T+2, so up to two consecutive
/// packet losses cost nothing.
const INPUT_REDUNDANCY: u64 = 3;

/// PostTick: send the local player's recent inputs to the server.
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
    let inputs: Vec<(u64, T)> = (tick.0.saturating_sub(INPUT_REDUNDANCY - 1)..=tick.0)
        .filter_map(|t| queue.get(t, local_player.0).map(|input| (t, input.clone())))
        .collect();
    if inputs.is_empty() {
        return;
    }
    commands.trigger(SendNetworkInput { inputs });
}
