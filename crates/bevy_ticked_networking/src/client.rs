use std::marker::PhantomData;

use bevy::prelude::*;

use bevy_ticked::{
    TickedLoop, TickedSimulation, TickedSystems,
    registry::TickedComponentRegistry,
    tick::{CurrentTick, TicksPaused},
    time::{run_tick_schedule, TickRateDilation},
    tracked_entity::{TickTrackedEntity, TickTrackedEntityCounter},
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
/// then solves directly for the lead that keeps a small positive margin and
/// converges toward it by dilating its tick rate (no transport RTT needed).
/// `target_ticks` is exposed for read-only display.
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
        crate::input::install_input_queue::<T>(app);
        app.init_resource::<ClientTickBuffer>()
            .add_message::<SnapshotApplied>()
            .add_observer(receive_snapshot)
            .add_systems(
                Update,
                reset_on_join::<T>.run_if(resource_added::<LocalClientPlayer>),
            )
            .add_systems(
                TickedLoop,
                (
                    handle_server_snapshot::<T>.in_set(TickedSystems::PreTick),
                    send_local_input::<T>.in_set(TickedSystems::PostTick),
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
///
/// Tracked entities are despawned here, and that is not tidiness. Zeroing the
/// counter while entities minted from the old one are still standing means the next
/// `next()` hands out an id that is already in use — and `apply_snapshot` keys the
/// whole world by id, so two entities sharing one id have their components merged
/// into whichever the client happens to hold. Whatever this peer built while it
/// thought it was playing alone is about to be replaced by the host's world in any
/// case, so there is nothing here worth keeping and every reason not to keep it.
///
/// [`reset_on_host`](crate::server::reset_on_host) closes the same hole the other
/// way, by raising the counter instead of despawning, because a solo player opening
/// their world to friends does have a claim on it.
fn reset_on_join<T: TickedInput>(world: &mut World) {
    let stale: Vec<Entity> = {
        let mut tracked = world.query_filtered::<Entity, With<TickTrackedEntity>>();
        tracked.iter(world).collect()
    };
    for entity in stale {
        world.despawn(entity);
    }

    world.insert_resource(CurrentTick(0));
    world.insert_resource(TicksPaused);
    world.insert_resource(TickTrackedEntityCounter::default());
    world.resource_mut::<InputQueue<T>>().inputs.clear();
    let registry = world.resource::<TickedComponentRegistry>().clone();
    registry.clear_all(world);
}

/// Written once a snapshot has been applied to the world.
///
/// A [`ReceivedNetworkSnapshot`](crate::messages::ReceivedNetworkSnapshot) says a
/// packet arrived; this says the world now reflects it, and — the part a consumer
/// cannot work out for itself — whether it was the initial sync.
///
/// The two are not alike and anything that eases, animates or announces has to
/// treat them differently: a correction moves a body centimetres, an initial sync
/// moves every body from wherever this peer imagined it to wherever it actually is.
/// Without this, consumers guess from the magnitude of the jump, which also catches
/// respawns and teleports and so is wrong in both directions.
#[derive(Message, Clone, Copy, Debug)]
pub struct SnapshotApplied {
    /// The tick the snapshot described.
    pub tick: u64,
    /// True for the initial sync, false for a steady-state correction.
    pub first: bool,
}

/// PreTick: if a server snapshot arrived, rollback and replay local inputs to now.
fn handle_server_snapshot<T: TickedInput>(world: &mut World) {
    let Some(pending) = world.remove_resource::<PendingSnapshot>() else {
        return;
    };

    // Not a client (yet). Applying the host's world at a peer that still thinks it
    // is playing alone gets everything downstream of the local player's uuid wrong
    // — which body is mine, which gets the camera, which is drawn as somebody else
    // — and this stack makes it likely rather than merely possible, because the
    // data channel comes up before the lobby is promoted.
    //
    // Dropped rather than held: the resource is removed above, so a snapshot that
    // arrives too early is discarded instead of waiting to be applied stale. They
    // are unreliable by construction, so losing one costs nothing.
    if !world.contains_resource::<LocalClientPlayer>() {
        return;
    }

    let was_paused = world.get_resource::<TicksPaused>().is_some();
    let current_tick = world.resource::<CurrentTick>().0;
    let snapshot_tick = pending.snapshot.tick;
    let tick_buffer = world.resource::<ClientTickBuffer>().target_ticks;

    let registry = world.resource::<TickedComponentRegistry>().clone();

    // Apply the authoritative snapshot (sets CurrentTick to snapshot_tick)
    apply_snapshot(world, &pending.snapshot);
    // `was_paused` is exactly "this is the initial sync". It used to be computed
    // here, used to decide whether to skip ahead, and thrown away; consumers were
    // left to infer it from how far bodies moved.
    world.write_message(SnapshotApplied {
        tick: snapshot_tick,
        first: was_paused,
    });

    if snapshot_tick >= current_tick {
        // Snapshot is at or ahead of us — jump forward.
        registry.capture_all(world, snapshot_tick);

        // On initial sync, skip ahead by tick_buffer so our inputs
        // arrive at the server before it reaches those ticks.
        if was_paused {
            let target_tick = snapshot_tick + tick_buffer;
            for tick in (snapshot_tick + 1)..=target_tick {
                world.resource_mut::<CurrentTick>().0 = tick;
                run_tick_schedule(world, tick, TickedSimulation);
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
    let end_tick = converge_lead(world, current_tick, lead, target);

    for tick in (snapshot_tick + 1)..=end_tick {
        world.resource_mut::<CurrentTick>().0 = tick;
        run_tick_schedule(world, tick, TickedSimulation);
        registry.capture_all(world, tick);
    }
    world.resource_mut::<CurrentTick>().0 = end_tick;
}

/// Largest deviation from the nominal tick rate used to steer the lead.
///
/// 2% is under the threshold where a rate change reads as motion artifact, and
/// small enough to stay stable on top of [`ClientTickBuffer`]'s EWMA — the two
/// together are a feedback loop, and a high gain here makes it hunt.
const MAX_DILATION: f64 = 0.02;

/// Lead error, in ticks, tolerated before correcting at all.
const LEAD_DEADBAND: f64 = 0.5;

/// Proportional gain: fraction of nominal rate corrected per tick of error.
const DILATION_GAIN: f64 = 0.01;

/// Tick-rate multiplier that corrects a lead error of `error` ticks.
///
/// Leading too much means running slow so the server catches up, and vice versa.
fn dilation_for(error: f64) -> f64 {
    if error.abs() < LEAD_DEADBAND {
        1.0
    } else {
        (1.0 - error * DILATION_GAIN).clamp(1.0 - MAX_DILATION, 1.0 + MAX_DILATION)
    }
}

/// Steer the prediction lead toward `target`, returning the tick to replay to.
///
/// Where an accumulator exists ([`TickSource::Hz`]), the correction is applied
/// as a small change to the tick *rate*: the client runs a couple of percent
/// fast or slow until the lead is right. That moves it relative to the server
/// continuously, and nothing in the simulation can tell. Adding or dropping a
/// whole tick corrects the same error in one frame, but every visual driven by
/// the simulation jumps by a tick when it happens.
///
/// Under [`TickSource::FixedUpdate`] there is no accumulator to stretch, so fall
/// back to the one-tick nudge rather than never converging.
///
/// [`TickSource::Hz`]: bevy_ticked::TickSource::Hz
/// [`TickSource::FixedUpdate`]: bevy_ticked::TickSource::FixedUpdate
fn converge_lead(world: &mut World, current_tick: u64, lead: u64, target: u64) -> u64 {
    let error = lead as f64 - target as f64;

    if let Some(mut dilation) = world.get_resource_mut::<TickRateDilation>() {
        dilation.0 = dilation_for(error);
        return current_tick;
    }

    // Deadband [target, target+1]; never drop below target, which would risk
    // inputs arriving after the server has passed their tick.
    if lead > target + 1 {
        current_tick - 1
    } else if lead < target {
        current_tick + 1
    } else {
        current_tick
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_lead_errors_are_ignored() {
        assert_eq!(dilation_for(0.0), 1.0);
        assert_eq!(dilation_for(0.4), 1.0);
        assert_eq!(dilation_for(-0.4), 1.0);
    }

    #[test]
    fn leading_too_much_slows_the_client_down() {
        assert!(dilation_for(1.0) < 1.0, "must run slow to shed lead");
        assert!(dilation_for(-1.0) > 1.0, "must run fast to gain lead");
    }

    #[test]
    fn correction_is_proportional_to_the_error() {
        let small = 1.0 - dilation_for(1.0);
        let large = 1.0 - dilation_for(2.0);
        assert!(large > small, "a bigger error must pull harder");
    }

    #[test]
    fn dilation_stays_within_the_clamp() {
        for error in [-1000.0, -50.0, -3.0, 3.0, 50.0, 1000.0] {
            let d = dilation_for(error);
            assert!(
                (1.0 - MAX_DILATION..=1.0 + MAX_DILATION).contains(&d),
                "error {error} produced {d}, outside the +/-2% clamp"
            );
        }
    }

    #[test]
    fn a_huge_error_never_stops_or_reverses_the_clock() {
        assert!(dilation_for(1e9) > 0.0, "the clock must keep moving forward");
    }
}
