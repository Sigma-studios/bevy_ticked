use std::marker::PhantomData;

use bevy::prelude::*;

use bevy_ticked::{
    TickedLoop, TickedSimulation, TickedSystems,
    events::TickedEventRegistry,
    registry::TickedComponentRegistry,
    resource_registry::TickedResourceRegistry,
    tick::{CurrentTick, HistoryBufferTicks, TickHoldReason, TickHolds},
    time::{run_tick_schedule, TickRateDilation},
    tracked_entity::{TickTrackedEntity, TickTrackedEntityCounter},
};

use crate::{
    diagnostics::{HealthWarnings, ReplayStats},
    input::{InputQueue, TickedInput},
    messages::{ReceivedNetworkSnapshot, SendNetworkInput},
    snapshot::{FullBody, SnapshotBody, SnapshotPacket, apply_full_body},
};

/// Resource identifying the local player on the client.
#[derive(Resource)]
pub struct LocalClientPlayer(pub u128);

/// Where in `TickedSystems::PreTick` a client's snapshot is applied, so a game or a plugin can
/// run before it (record what the renderer showed) and after it (measure what changed).
#[derive(SystemSet, Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ClientSet {
    /// Before the pending snapshot is applied and replayed.
    BeforeSnapshot,
    /// `handle_server_snapshot`: apply, roll back, replay.
    ApplySnapshot,
    /// After the replay, still before the tick.
    AfterSnapshot,
}

/// The newest snapshot that has arrived and not yet been applied.
///
/// Written in place from the observer rather than inserted through `Commands`:
/// two snapshots arriving in one frame have to compare against each other, and
/// a deferred insert leaves both of them comparing against an empty slot.
#[derive(Resource, Default)]
struct PendingSnapshot(Option<SnapshotPacket>);

/// The tick of the last snapshot this client applied, so an older one arriving
/// later is recognised for what it is.
///
/// Snapshots go over an unordered, no-retransmit channel — which is right, and
/// means tick 100 can arrive before tick 99. The late 99 used to be applied as
/// if it were news: a rollback to a state the authority had already moved past,
/// a replay from there, a pellet the host despawned at 100 standing again for a
/// frame, and `TickTrackedEntityCounter` set backwards to 99's high-water mark.
/// Nothing anybody would attribute to packet reordering. Anything at or before
/// this tick is dropped at the door now, before it can become the pending one.
///
/// `None` until the first snapshot of a session, and reset with the session:
/// the clock restarts at zero, so a tick remembered from the last session would
/// make every snapshot of the next one look old.
#[derive(Resource, Default, Debug, Clone, Copy)]
pub struct AppliedSnapshotTick(pub Option<u64>);

/// The tick of the snapshot waiting to be applied this pass, if any. Read by measurements
/// that want to sample the prediction for that tick before it is overwritten.
#[derive(Resource, Default, Debug, Clone, Copy)]
pub struct PendingSnapshotTick(pub Option<u64>);

/// `seq` of the newest snapshot this client applied, acknowledged on every input packet.
#[derive(Resource, Default, Debug, Clone, Copy)]
pub struct LastAppliedSeq(pub Option<u32>);

/// How many ticks ahead of the server the client runs (its prediction lead).
///
/// The client must lead the server by enough that its inputs arrive before the
/// server reaches the tick they're for. This sizes itself from the *actual* input
/// timeliness, self-contained in this crate: the server measures how many ticks
/// early/late each client's inputs arrive and reports it in every snapshot (see
/// [`WorldSnapshot::input_margins`](crate::snapshot::WorldSnapshot)); the client
/// then solves directly for the lead that keeps a small positive margin and
/// converges toward it by dilating its tick rate (no transport RTT needed).
/// `target_replay_distance` is exposed for read-only display.
///
/// # It is a replay distance, not a lead
///
/// The name is the whole warning. `target_replay_distance` is what
/// `current_tick - snapshot_tick` should settle at, and a snapshot is one
/// one-way trip old by the time it is read — so the *lead* this buffer actually
/// holds is `target_replay_distance - one_way`, and in steady state that is
/// `one_way + target_margin`. Which is correct: the margin is what matters, and
/// it lands on [`target_margin`](Self::target_margin) exactly.
///
/// It used to be called `target_ticks` and documented as the lead, which it has
/// never been. Nothing downstream was wrong by it — [`observe`](Self::observe)
/// solves for the same quantity it is compared against, so the units agree with
/// each other even though neither matched the name — but every readout built on
/// it reported a number a third larger than the lead it claimed to be, and the
/// one test that asserted on it needed an eight-tick tolerance to pass.
#[derive(Resource, Clone, Copy, Debug)]
pub struct ClientTickBuffer {
    /// Target replay distance, in ticks: what `current_tick - snapshot_tick`
    /// converges to. See the note on the type — this is not the lead.
    pub target_replay_distance: u64,
    /// Desired input-arrival margin: inputs should reach the server this many
    /// ticks early, to absorb jitter and once-per-frame delivery.
    ///
    /// A field rather than a constant because the right value is a property of
    /// the link: it has to cover how late an *unlucky* packet is, not an average
    /// one, and a link with 40 ms of jitter needs more than two ticks of it. A
    /// transport that measures jitter can size this with
    /// [`seed_from_rtt`](Self::seed_from_rtt); [`DEFAULT_MARGIN`](Self::DEFAULT_MARGIN)
    /// is the floor and the fallback.
    pub target_margin: i64,
    /// EWMA accumulator for the target, so per-snapshot margin jitter doesn't
    /// make it wander.
    smoothed: f64,
}

impl Default for ClientTickBuffer {
    fn default() -> Self {
        // Until a margin has been measured or a transport has seeded one. Six
        // ticks of replay distance is a 62 ms round trip — see `seed_from_rtt`
        // for why guessing here is survivable but not free.
        Self {
            target_replay_distance: 6,
            target_margin: Self::DEFAULT_MARGIN,
            smoothed: 6.0,
        }
    }
}

impl ClientTickBuffer {
    /// Input-arrival margin used until something measures a better one.
    pub const DEFAULT_MARGIN: i64 = 2;
    /// Floor on the margin. Below two ticks there is no room for once-per-frame
    /// delivery, let alone jitter.
    pub const MIN_MARGIN: i64 = 2;
    /// Ceiling on the margin, so a pathological jitter measurement cannot spend
    /// the whole prediction budget on headroom.
    pub const MAX_MARGIN: i64 = 12;
    /// Never target less replay distance than this.
    const MIN_TICKS: u64 = 2;
    /// Cap it so a pathological connection can't make prediction explode.
    const MAX_TICKS: u64 = crate::input::MAX_INPUT_LEAD_TICKS;
    /// EWMA weight for new observations.
    const SMOOTHING: f64 = 0.1;

    /// Update the target from an observed replay distance
    /// (`current_tick - snapshot_tick`) and the server-measured input margin.
    ///
    /// With `replay_distance = lead + one_way` and `margin = lead - one_way`,
    /// `replay_distance - margin + target_margin` is `2 * one_way + target_margin`
    /// — the replay distance at which the margin comes out at `target_margin`.
    /// A stable fixed point, EWMA-smoothed against jitter.
    ///
    /// `replay_distance` is signed because a client that has fallen *behind* the
    /// authority has a negative one, and that is exactly when this most needs to
    /// keep tracking. It used to take a `u64`, so the one caller that could
    /// supply a negative value could not call it at all, and the target froze at
    /// whatever it last saw for as long as the client was behind — which was
    /// forever, because being behind is self-sustaining.
    fn observe(&mut self, replay_distance: i64, margin: i64) {
        let raw = (replay_distance - margin + self.target_margin)
            .clamp(Self::MIN_TICKS as i64, Self::MAX_TICKS as i64) as f64;
        self.smoothed = (1.0 - Self::SMOOTHING) * self.smoothed + Self::SMOOTHING * raw;
        self.target_replay_distance =
            (self.smoothed.round() as u64).clamp(Self::MIN_TICKS, Self::MAX_TICKS);
    }

    /// Size the target from a measured round trip and round-trip jitter, before
    /// any input has made the trip for [`observe`](Self::observe) to read.
    ///
    /// The steady state `observe` converges to is `rtt + target_margin` ticks, so
    /// that is what this sets — and the margin itself comes from the jitter,
    /// because the margin's whole job is to cover the packets that arrive late
    /// rather than the ones that arrive on time.
    ///
    /// This crate has no transport and cannot measure either value; a bridge that
    /// has one calls this. Skipping it is survivable — `observe` converges within
    /// a second or so of the first input — but the default is a guess, and a link
    /// whose one-way trip is longer than that guess starts the session with the
    /// client already behind.
    ///
    /// Jitter is passed as *round-trip* variation and used as-is, which is roughly
    /// twice the one-way figure the margin actually needs. Deliberately generous:
    /// excess margin costs a little replay depth, and too little costs dropped
    /// input.
    pub fn seed_from_rtt(
        &mut self,
        round_trip: core::time::Duration,
        round_trip_jitter: core::time::Duration,
        timestep: core::time::Duration,
    ) {
        let in_ticks = |d: core::time::Duration| {
            (d.as_secs_f64() / timestep.as_secs_f64().max(f64::EPSILON)).ceil()
        };
        self.target_margin =
            (in_ticks(round_trip_jitter) as i64).clamp(Self::MIN_MARGIN, Self::MAX_MARGIN);
        let raw = (in_ticks(round_trip) + self.target_margin as f64)
            .clamp(Self::MIN_TICKS as f64, Self::MAX_TICKS as f64);
        self.smoothed = raw;
        self.target_replay_distance = raw.round() as u64;
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
        crate::replication::install_owner(app);
        // A rollback never reaches further back than one one-way trip plus the lead, and the
        // lead is capped at `MAX_TICKS`; twice that is every tick a snapshot could still name.
        // The core default is a hundred seconds, sized for scrubbing, and on a client that was
        // a hundred seconds of every registered component kept for a rewind that cannot
        // happen — and walked on every capture.
        if !app.world().contains_resource::<bevy_ticked::HistoryWindowChosen>() {
            app.insert_resource(HistoryBufferTicks(2 * ClientTickBuffer::MAX_TICKS));
        }
        app.init_resource::<ClientTickBuffer>()
            .init_resource::<PendingSnapshot>()
            .init_resource::<AppliedSnapshotTick>()
            .init_resource::<PendingSnapshotTick>()
            .init_resource::<LastAppliedSeq>()
            .init_resource::<ReplayStats>()
            .init_resource::<HealthWarnings>()
            .add_message::<SnapshotApplied>()
            .add_observer(receive_snapshot)
            .add_systems(
                Update,
                reset_on_join::<T>.run_if(resource_added::<LocalClientPlayer>),
            )
            .add_systems(
                Update,
                crate::reset_on_leave::<T>.run_if(resource_removed::<LocalClientPlayer>),
            )
            .init_resource::<CounterAfterSnapshot>()
            .configure_sets(
                TickedLoop,
                (
                    ClientSet::BeforeSnapshot,
                    ClientSet::ApplySnapshot,
                    ClientSet::AfterSnapshot,
                )
                    .chain()
                    .in_set(TickedSystems::PreTick),
            )
            .add_plugins(crate::replication::RemoteInterpolationPlugin)
            .add_systems(
                TickedLoop,
                (
                    handle_server_snapshot::<T>.in_set(ClientSet::ApplySnapshot),
                    (send_local_input::<T>, watch_for_client_minted_ids)
                        .in_set(TickedSystems::PostTick),
                ),
            );
    }

    fn finish(&self, app: &mut App) {
        bevy_ticked::require_steerable_tick_source(app, "TickedClientPlugin");
    }
}

/// Observer: store incoming server snapshot for processing before the next tick.
///
/// Only if it is newer than the last one applied *and* the one already waiting.
/// Two snapshots can land in one frame in either order, and the waiting slot used
/// to be "last writer wins" — so 100 then 99 kept 99, and the client rolled back
/// to a state the authority had already left. See [`AppliedSnapshotTick`].
fn receive_snapshot(
    trigger: On<ReceivedNetworkSnapshot>,
    applied: Res<AppliedSnapshotTick>,
    mut pending: ResMut<PendingSnapshot>,
    mut pending_tick: ResMut<PendingSnapshotTick>,
    mut stats: ResMut<ReplayStats>,
    mut history: ResMut<crate::replication::AuthoritativeHistory>,
) {
    let tick = trigger.event().0.tick;
    let newest_seen = applied
        .0
        .into_iter()
        .chain(pending.0.as_ref().map(|waiting| waiting.tick))
        .max();
    if newest_seen.is_some_and(|newest| tick <= newest) {
        stats.dropped_stale += 1;
        // Stale for the rollback, still the authority's word about that tick: an interpolated
        // entity drawn from the history would otherwise see a hole where the late packet was
        // and jump across it.
        if let SnapshotBody::Full(body) = &trigger.event().0.body
            && history.oldest_tick().is_none_or(|oldest| tick >= oldest)
            && !history.has_tick(tick)
        {
            history.record(tick, body.entities.iter().cloned());
        }
        return;
    }
    // A packet still waiting when a newer one lands is superseded for the rollback, and kept
    // for the drawn path like a stale one: three snapshots in one frame used to leave two
    // holes in the history, and an interpolated body jumped across them.
    if let Some(superseded) = pending.0.replace(trigger.event().0.clone())
        && let SnapshotBody::Full(body) = &superseded.body
        && !history.has_tick(superseded.tick)
    {
        history.record(superseded.tick, body.entities.iter().cloned());
    }
    pending_tick.0 = Some(tick);
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
    // Held until the host's world arrives; released by the first snapshot. A game's own pause
    // is a different reason and is neither set nor lifted here.
    world
        .resource_mut::<TickHolds>()
        .hold(TickHoldReason::AwaitingSync);
    world.insert_resource(TickTrackedEntityCounter::default());
    world.insert_resource(AppliedSnapshotTick::default());
    world.insert_resource(LastAppliedSeq::default());
    world.resource_mut::<InputQueue<T>>().inputs.clear();
    if let Some(mut history) = world.get_resource_mut::<crate::replication::AuthoritativeHistory>() {
        history.clear();
    }
    world.insert_resource(crate::replication::DisplayTick::default());
    let registry = world.resource::<TickedComponentRegistry>().clone();
    registry.clear_all(world);
    // Registered resources go back to their defaults: the host's first snapshot brings the
    // networked ones, and the local ones have no business carrying a previous session's value.
    if let Some(resources) = world.get_resource::<TickedResourceRegistry>().cloned() {
        resources.reset_all(world);
    }
    TickedEventRegistry::clear_all(world);
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

/// A replay that did not fit in one frame: the tick it is heading for. While present the
/// clock holds [`TickHoldReason::Replaying`] and each loop pass runs up to `MaxTicksPerFrame`
/// more of it. A new snapshot supersedes it.
#[derive(Resource, Debug, Clone, Copy, PartialEq, Eq)]
pub struct AwaitingReplay {
    pub end_tick: u64,
}

/// Run ticks `from + 1 ..= end_tick`, at most `budget` of them, capturing each. Returns the
/// tick reached.
fn replay_ticks(
    world: &mut World,
    registry: &TickedComponentRegistry,
    from: u64,
    end_tick: u64,
    budget: u64,
) -> u64 {
    let stop = end_tick.min(from + budget);
    for tick in (from + 1)..=stop {
        world.resource_mut::<CurrentTick>().0 = tick;
        run_tick_schedule(world, tick, TickedSimulation);
        registry.capture_all(world, tick);
    }
    world.resource_mut::<CurrentTick>().0 = stop;
    stop
}

/// Replay toward `end_tick` from the current tick, within this frame's budget; if it is not
/// finished, leave [`AwaitingReplay`] and hold the clock so the next pass continues it.
fn replay_bounded(world: &mut World, registry: &TickedComponentRegistry, end_tick: u64) {
    let from = world.resource::<CurrentTick>().0;
    let budget = u64::from(world.resource::<bevy_ticked::MaxTicksPerFrame>().0);
    let reached = replay_ticks(world, registry, from, end_tick, budget);
    {
        let mut stats = world.resource_mut::<ReplayStats>();
        stats.ticks_replayed += reached.saturating_sub(from);
    }
    let mut holds = world.resource_mut::<TickHolds>();
    if reached < end_tick {
        holds.hold(TickHoldReason::Replaying);
        world.insert_resource(AwaitingReplay { end_tick });
    } else {
        holds.release(TickHoldReason::Replaying);
        world.remove_resource::<AwaitingReplay>();
    }
}

/// PreTick: if a server snapshot arrived, rollback and replay local inputs to now.
fn handle_server_snapshot<T: TickedInput>(world: &mut World) {
    let Some(packet) = world.resource_mut::<PendingSnapshot>().0.take() else {
        // Nothing new: carry on with a replay that did not fit in the last frame.
        if let Some(awaiting) = world.get_resource::<AwaitingReplay>().copied() {
            let registry = world.resource::<TickedComponentRegistry>().clone();
            replay_bounded(world, &registry, awaiting.end_tick);
        }
        return;
    };
    world.resource_mut::<PendingSnapshotTick>().0 = None;
    // A new snapshot supersedes a replay in progress: the rollback below starts over from it.
    world.remove_resource::<AwaitingReplay>();
    world
        .resource_mut::<TickHolds>()
        .release(TickHoldReason::Replaying);
    let body = match &packet.body {
        SnapshotBody::Full(body) => body,
        SnapshotBody::Delta(_) => {
            // Reserved for the delta phase. A host from after it talking to a client from
            // before would be refused at the handshake, so this is a defence, not a path.
            world.resource_mut::<ReplayStats>().dropped_delta_body += 1;
            return;
        }
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
        world.resource_mut::<ReplayStats>().dropped_before_handshake += 1;
        return;
    }

    // "This is the initial sync": the client is still waiting for the world it joined.
    let was_paused = world
        .resource::<TickHolds>()
        .holds(TickHoldReason::AwaitingSync);
    let current_tick = world.resource::<CurrentTick>().0;
    let snapshot_tick = packet.tick;

    let registry = world.resource::<TickedComponentRegistry>().clone();

    // The fast path: the authority agrees with the prediction, so there is nothing to
    // correct and nothing to replay. The comparison is exact — a prediction off by an ulp is
    // one that will drift, and the replay is how it is put right — and it costs a decode of
    // the packet, which the slow path pays anyway. Before this a client replayed its whole
    // lead on every snapshot: seven simulation runs per frame on a world where nothing had
    // happened, and every `Changed<T>` and observer firing seven times for it.
    if !was_paused
        && snapshot_tick < current_tick
        && prediction_matches(world, &registry, body, snapshot_tick)
    {
        accept_identical::<T>(world, &packet, body, current_tick);
        return;
    }

    // A snapshot older than anything still in history can be applied, but the replay from it
    // cannot restore rollback-only state for the ticks in between: they are gone. Counted, and
    // said once, because it is the kind of thing that presents as "the client is slightly off"
    // for a session and has an exact cause.
    if let Some(oldest) = registry.oldest_captured_tick(world)
        && snapshot_tick < oldest
    {
        let mut health = world.resource_mut::<HealthWarnings>();
        let mut count = health.snapshot_older_than_history;
        HealthWarnings::raise(&mut count, || {
            format!(
                "a snapshot for tick {snapshot_tick} arrived, but the oldest tick still in \
                 history is {oldest}: rollback-only state between them cannot be restored. Raise \
                 HistoryBufferTicks or lower the lead."
            )
        });
        health.snapshot_older_than_history = count;
    }

    // Apply the authoritative snapshot (sets CurrentTick to snapshot_tick)
    let applied = apply_full_body(world, snapshot_tick, body);
    world
        .resource_mut::<crate::replication::AuthoritativeHistory>()
        .record(snapshot_tick, body.entities.iter().cloned());
    if !applied.duplicate_ids.is_empty() {
        let mut health = world.resource_mut::<HealthWarnings>();
        let mut count = health.duplicate_ids_in_snapshot;
        HealthWarnings::raise(&mut count, || {
            format!(
                "a snapshot for tick {snapshot_tick} named the same tracked id more than once: \
                 {:?}. Only the first record was applied.",
                applied.duplicate_ids
            )
        });
        health.duplicate_ids_in_snapshot = count;
    }
    file_relayed_inputs::<T>(world, body);
    world.insert_resource(AppliedSnapshotTick(Some(snapshot_tick)));
    world.insert_resource(LastAppliedSeq(Some(packet.seq)));
    {
        let mut stats = world.resource_mut::<ReplayStats>();
        stats.snapshots_applied += 1;
        stats.last_replay_distance = current_tick as i64 - snapshot_tick as i64;
    }
    // `was_paused` is exactly "this is the initial sync". It used to be computed
    // here, used to decide whether to skip ahead, and thrown away; consumers were
    // left to infer it from how far bodies moved.
    world.write_message(SnapshotApplied {
        tick: snapshot_tick,
        first: was_paused,
    });

    // Self-adaptive target: update it from the server-reported input margin for
    // this client (how early or late its inputs are arriving), self-contained in
    // this crate.
    //
    // Before the branch, and not inside the rollback arm where it used to live.
    // The arm below runs precisely when the client is behind and its inputs are
    // being dropped, which is when the target most needs to keep moving; leaving
    // the observation out of it froze the target at its last value for exactly as
    // long as the problem lasted. Skipped on the initial sync, where the tick
    // difference is "however long this peer has been running" rather than a
    // measurement, and no input has been sent for the server to have timed.
    let replay_distance = current_tick as i64 - snapshot_tick as i64;
    if !was_paused {
        world
            .resource_mut::<ClientTickBuffer>()
            .observe(replay_distance, i64::from(packet.your_margin));
    }

    if snapshot_tick >= current_tick {
        // At or behind the authority: there is nothing to roll back, and no
        // prediction lead left. **Acquire one outright.**
        //
        // This arm used to apply the snapshot and return unless it was the
        // initial sync, and that was the single worst bug in this crate. Every
        // input a client sends is stamped with its own tick, and the server reads
        // only the entry for the tick it is about to run — so a client with no
        // lead has every input it will ever send arrive too late to be read.
        // Worse, the state is self-sustaining: `apply_snapshot` sets `CurrentTick`
        // to the snapshot's, so the client is pinned exactly one one-way trip
        // behind, every subsequent snapshot takes this same arm, and nothing here
        // ever measured, steered or escaped. One dropped frame — a backgrounded
        // tab, a shader compile, a collection — and the player could not move
        // again for the rest of the session while everyone else moved normally.
        //
        // Simulating forward is a visible discontinuity and it is the right
        // trade: the client has genuinely lost this time, and the alternative is
        // being frozen out of the session permanently. Rate dilation cannot do
        // this job — at two percent it closes a half-second hole in twenty
        // seconds, and the next snapshot re-pins it long before then.
        //
        // It self-limits. After the jump the client leads again, so the next
        // snapshot lands behind it and the ordinary rollback path resumes.
        registry.capture_all(world, snapshot_tick);

        let target = world.resource::<ClientTickBuffer>().target_replay_distance;
        world.resource_mut::<ReplayStats>().rollbacks += 1;
        world
            .resource_mut::<TickHolds>()
            .release(TickHoldReason::AwaitingSync);
        replay_bounded(world, &registry, snapshot_tick + target);
        // The lead was just set outright, so there is no error left for the rate
        // trim to work on. Leaving a stale value here is not harmless: a client
        // that was shedding lead at 0.98 when it fell behind would keep running
        // slow, losing the lead it had just been given.
        if let Some(mut dilation) = world.get_resource_mut::<TickRateDilation>() {
            dilation.0 = 1.0;
        }
        return;
    }

    // Paused and already ahead of the snapshot. Not reachable after a normal
    // `reset_on_join`, which zeroes the tick before any snapshot can arrive, but
    // replaying while paused is not a thing to start doing if it ever is.
    if was_paused {
        registry.capture_all(world, snapshot_tick);
        world
            .resource_mut::<TickHolds>()
            .release(TickHoldReason::AwaitingSync);
        return;
    }

    // Snapshot is behind us: roll back and replay predicted ticks.
    //
    // The networked half of the world is now the authority's, written by `apply_snapshot`.
    // The local half — components and resources registered without a wire name, which only
    // roll back — is put back to what it was at the snapshot's tick, from history; it used to
    // keep the value of the client's last predicted tick, so the replay started from a world
    // that was part authority and part future. Then everything after the snapshot's tick is
    // forgotten: the component history, and the event logs, or a prediction that never
    // happened stays presented and its correction is swallowed as "already shown".
    registry.restore_local_only(world, snapshot_tick);
    registry.truncate_all_after(world, snapshot_tick);
    TickedEventRegistry::truncate_all_after(world, snapshot_tick);

    let target = world.resource::<ClientTickBuffer>().target_replay_distance;
    let end_tick = converge_lead(world, current_tick, replay_distance as u64, target);

    world.resource_mut::<ReplayStats>().rollbacks += 1;
    replay_bounded(world, &registry, end_tick);
}

/// Whether the authority's body for `tick` is exactly what this client predicted for it.
///
/// The set of tracked ids must be the same (a spawn or a despawn is a correction), every
/// predicted entity's networked components must be present in the same set and equal to the
/// values captured at `tick`, and the networked resources must encode to the same bytes.
/// Interpolated entities are not compared: they are never predicted, so their record is
/// simply what will be drawn. Anything that fails to decode is a mismatch, and the slow path
/// reports it.
fn prediction_matches(
    world: &mut World,
    registry: &TickedComponentRegistry,
    body: &FullBody,
    tick: u64,
) -> bool {
    let local: std::collections::HashMap<u64, bool> = {
        let mut query = world.query::<(&TickTrackedEntity, Option<&crate::replication::ReplicationMode>)>();
        query
            .iter(world)
            .map(|(tracked, mode)| {
                (
                    tracked.0,
                    matches!(mode, Some(crate::replication::ReplicationMode::Predicted)),
                )
            })
            .collect()
    };
    if local.len() != body.entities.len() {
        return false;
    }
    for record in &body.entities {
        let Some(predicted) = local.get(&record.id) else {
            return false;
        };
        if !predicted {
            continue;
        }
        if registry.saved_wire_types_at(world, tick, record.id) != record.present {
            return false;
        }
        let mut rest: &[u8] = &record.bytes;
        for wire_index in record.present.iter() {
            match registry.matches_at(world, wire_index, tick, record.id, rest) {
                Some((true, consumed)) => rest = &rest[consumed..],
                _ => return false,
            }
        }
    }
    let ours = world
        .get_resource::<TickedResourceRegistry>()
        .cloned()
        .map(|resources| resources.serialize_all(world, tick))
        .unwrap_or_default();
    ours == body.resources
}

/// The bookkeeping of applying a snapshot, for one that changed nothing: the history, the
/// acks, the relayed inputs, the margin — and the lead, which may still need a step.
fn accept_identical<T: TickedInput>(
    world: &mut World,
    packet: &SnapshotPacket,
    body: &FullBody,
    current_tick: u64,
) {
    let snapshot_tick = packet.tick;
    world
        .resource_mut::<crate::replication::AuthoritativeHistory>()
        .record(snapshot_tick, body.entities.iter().cloned());
    file_relayed_inputs::<T>(world, body);
    world.insert_resource(AppliedSnapshotTick(Some(snapshot_tick)));
    world.insert_resource(LastAppliedSeq(Some(packet.seq)));
    {
        let mut stats = world.resource_mut::<ReplayStats>();
        stats.snapshots_applied += 1;
        stats.skipped_identical += 1;
        stats.last_replay_distance = current_tick as i64 - snapshot_tick as i64;
    }
    world.write_message(SnapshotApplied {
        tick: snapshot_tick,
        first: false,
    });
    let replay_distance = current_tick as i64 - snapshot_tick as i64;
    world
        .resource_mut::<ClientTickBuffer>()
        .observe(replay_distance, i64::from(packet.your_margin));
    let target = world.resource::<ClientTickBuffer>().target_replay_distance;
    let end_tick = converge_lead(world, current_tick, replay_distance as u64, target);
    // A lead deficit is taken forward, as a plain simulation of the missing ticks; an excess
    // is left to the rate trim, since going back would be a rewind of correct ticks.
    if end_tick > current_tick {
        let registry = world.resource::<TickedComponentRegistry>().clone();
        replay_bounded(world, &registry, end_tick);
    }
}

/// Other players' inputs the host already holds. The local player's own are dropped: the
/// client has them, and a relayed copy could be older than what it has queued since.
fn file_relayed_inputs<T: TickedInput>(world: &mut World, body: &FullBody) {
    if body.inputs_ahead.is_empty() {
        return;
    }
    let local = world.get_resource::<LocalClientPlayer>().map(|p| p.0);
    let mut queue = world.resource_mut::<InputQueue<T>>();
    for relayed in &body.inputs_ahead {
        if Some(relayed.player) == local {
            continue;
        }
        if let Ok(input) = postcard::from_bytes::<T>(&relayed.bytes) {
            queue.insert(relayed.tick, relayed.player, input);
        }
    }
}

/// Largest deviation from the nominal tick rate used to steer the lead.
///
/// 2% is under the threshold where a rate change reads as motion artifact, and
/// small enough to stay stable on top of [`ClientTickBuffer`]'s EWMA — the two
/// together are a feedback loop, and a high gain here makes it hunt.
///
/// It is deliberately *not* the tool for a large error. Two percent of 64 Hz is
/// 1.28 ticks per second, so an eight-tick hole takes six seconds to close —
/// during which the client's input is arriving late and being dropped. Raising
/// the ceiling is the wrong answer to that; [`SNAP_TICKS`] is.
const MAX_DILATION: f64 = 0.02;

/// Lead error, in ticks, tolerated before correcting at all.
const LEAD_DEADBAND: f64 = 0.5;

/// Proportional gain: fraction of nominal rate corrected per tick of error.
const DILATION_GAIN: f64 = 0.01;

/// Deficit, in ticks, past which the lead is taken in one step instead of
/// dilated toward.
///
/// Below this the rate trim converges in about a second and is invisible, which
/// is what it is for. Above it the trim is slower than the disturbances that
/// create the error, so it never arrives — and every tick spent short of the
/// target is a tick of input the server may drop.
///
/// Forward only. An *excess* lead costs a deeper replay and nothing else, and
/// shedding it by rewinding the clock would be a visible jump to fix a problem
/// nobody can see.
const SNAP_TICKS: f64 = 4.0;

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

/// Steer the replay distance toward `target`, returning the tick to replay to.
///
/// Where an accumulator exists ([`TickSource::Hz`]), the correction is applied
/// as a small change to the tick *rate*: the client runs a couple of percent
/// fast or slow until the lead is right. That moves it relative to the server
/// continuously, and nothing in the simulation can tell. Adding or dropping a
/// whole tick corrects the same error in one frame, but every visual driven by
/// the simulation jumps by a tick when it happens.
///
/// Under [`TickSource::Manual`] there is no accumulator to stretch — whoever drives
/// the loop owns the pacing — so fall back to the one-tick nudge rather than never
/// converging. `FixedUpdate` is refused at build time (see
/// [`require_steerable_tick_source`](bevy_ticked::require_steerable_tick_source)).
///
/// [`TickSource::Hz`]: bevy_ticked::TickSource::Hz
/// [`TickSource::Manual`]: bevy_ticked::TickSource::Manual
fn converge_lead(world: &mut World, current_tick: u64, replay_distance: u64, target: u64) -> u64 {
    let error = replay_distance as f64 - target as f64;

    // Too far short for the rate trim to close before the next disturbance, or
    // before falling behind entirely. Take it in one step.
    if error <= -SNAP_TICKS {
        if let Some(mut dilation) = world.get_resource_mut::<TickRateDilation>() {
            dilation.0 = 1.0;
        }
        return current_tick + (target - replay_distance);
    }

    if let Some(mut dilation) = world.get_resource_mut::<TickRateDilation>() {
        dilation.0 = dilation_for(error);
        return current_tick;
    }

    // Deadband [target, target+1]; never drop below target, which would risk
    // inputs arriving after the server has passed their tick.
    if replay_distance > target + 1 {
        current_tick - 1
    } else if replay_distance < target {
        current_tick + 1
    } else {
        current_tick
    }
}

/// The counter as the last snapshot left it, so a client that mints an id on its own is caught.
///
/// Until predicted spawns land, only the authority may mint a tracked id: a client that does so
/// hands out a number the host will hand out too, and `apply_snapshot` then merges two entities
/// into one. Every consumer wrote a `debug_assert` for this; here it is once, as a warning that
/// names the id.
#[derive(Resource, Default)]
struct CounterAfterSnapshot(u64);

fn watch_for_client_minted_ids(
    counter: Res<TickTrackedEntityCounter>,
    applied: Res<AppliedSnapshotTick>,
    mut after_snapshot: ResMut<CounterAfterSnapshot>,
    mut health: ResMut<HealthWarnings>,
) {
    if applied.is_changed() {
        after_snapshot.0 = counter.0;
        return;
    }
    if counter.0 > after_snapshot.0 {
        let minted = counter.0;
        HealthWarnings::raise(&mut health.client_minted_tracked_id, || {
            format!(
                "this client minted tracked id {minted} itself; only the authority may, or the \
                 host will hand the same id to something else"
            )
        });
        after_snapshot.0 = counter.0;
    }
}

/// Number of recent ticks of input included in each packet. Input for tick T
/// also rides in the packets sent at T+1 and T+2, so up to two consecutive
/// packet losses cost nothing.
const INPUT_REDUNDANCY: u64 = 3;

/// PostTick: send the local player's recent inputs to the server.
fn send_local_input<T: TickedInput>(
    tick: Res<CurrentTick>,
    holds: Res<TickHolds>,
    local_player: Option<Res<LocalClientPlayer>>,
    queue: Res<InputQueue<T>>,
    ack: Res<LastAppliedSeq>,
    mut commands: Commands,
) {
    if holds.is_held() {
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
    commands.trigger(SendNetworkInput {
        inputs,
        ack: ack.0,
    });
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

    #[test]
    fn a_large_deficit_is_snapped_rather_than_dilated() {
        // The reason the ceiling above is allowed to stay small. 2% of 64 Hz is 1.28
        // ticks a second, so anything past a few ticks has to be taken in one step or
        // the next disturbance arrives before the correction does.
        const { assert!(SNAP_TICKS > LEAD_DEADBAND) };
        let saturates_at = MAX_DILATION / DILATION_GAIN;
        assert!(
            SNAP_TICKS >= saturates_at,
            "dilation saturates at {saturates_at} ticks of error, so snapping before that \
             would take away errors the trim can still handle"
        );
    }

    /// A client that has fallen behind, expressed the way `handle_server_snapshot` does.
    fn behind(one_way: i64, lead: i64) -> (i64, i64) {
        (lead + one_way, lead - one_way)
    }

    #[test]
    fn the_target_keeps_tracking_while_the_client_is_behind() {
        // The observation used to take a `u64`, so the one caller that could pass a
        // negative replay distance could not call it at all — and being behind is the
        // state that most needs the target to keep moving, because it is self-sustaining.
        let mut buffer = ClientTickBuffer::default();
        let (replay_distance, margin) = behind(6, -2);
        for _ in 0..200 {
            buffer.observe(replay_distance, margin);
        }
        // 2 * one_way + margin.
        assert_eq!(buffer.target_replay_distance, 14);
    }

    #[test]
    fn seeding_lands_where_observing_would_have_converged() {
        // The point of seeding: arrive at the answer the feedback loop would have found,
        // without waiting for the round trip that teaches it. If these two ever drift
        // apart, a seeded client gets corrected the moment its first input is timed.
        let timestep = core::time::Duration::from_secs_f64(1.0 / 64.0);
        let one_way_ticks = 4;
        let round_trip = timestep * (one_way_ticks as u32 * 2);

        let mut seeded = ClientTickBuffer::default();
        seeded.seed_from_rtt(round_trip, core::time::Duration::ZERO, timestep);

        let mut observed = ClientTickBuffer::default();
        let (replay_distance, margin) = behind(one_way_ticks, seeded.target_margin);
        for _ in 0..500 {
            observed.observe(replay_distance, margin);
        }

        assert_eq!(
            seeded.target_replay_distance, observed.target_replay_distance,
            "seeding and observing disagree about the same link"
        );
    }

    #[test]
    fn a_jittery_link_is_given_more_margin_than_a_clean_one() {
        // The margin is headroom for the *unlucky* packet. Two ticks of it is under water
        // on a link with 40 ms of variation, which is where the input loss came from.
        let timestep = core::time::Duration::from_secs_f64(1.0 / 64.0);
        let round_trip = core::time::Duration::from_millis(60);

        let mut clean = ClientTickBuffer::default();
        clean.seed_from_rtt(round_trip, core::time::Duration::ZERO, timestep);

        let mut jittery = ClientTickBuffer::default();
        jittery.seed_from_rtt(round_trip, core::time::Duration::from_millis(40), timestep);

        assert_eq!(clean.target_margin, ClientTickBuffer::MIN_MARGIN);
        assert!(
            jittery.target_margin > clean.target_margin,
            "jitter bought no extra headroom: {} against {}",
            jittery.target_margin,
            clean.target_margin
        );
        assert!(jittery.target_margin <= ClientTickBuffer::MAX_MARGIN);
        assert!(
            jittery.target_replay_distance > clean.target_replay_distance,
            "the extra margin has to show up in the distance the client actually keeps"
        );
    }
}
