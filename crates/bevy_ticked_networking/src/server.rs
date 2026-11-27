use std::collections::{BTreeSet, HashMap};
use std::marker::PhantomData;

use bevy::prelude::*;

use bevy_ticked::{
    TickedLoop, TickedSystems,
    lifetimes::TrackedEntityLifetimes,
    registry::TickedComponentRegistry,
    session::{PerPeer, SessionAppExt, SessionReset, SessionScope},
    tick::{CurrentTick, HistoryBufferTicks, TickHoldReason, TickHolds},
    tick_types::{Tick, Ticks},
};

use crate::{
    delta::{Baseline, Baselines, Compression, DeltaPolicy, SendRates, build_delta, layouts_of},
    diagnostics::{InputStats, SnapshotStats},
    input::{InputQueue, LateInputFold, MAX_INPUT_LEAD_TICKS, TickedInput},
    messages::{ReceivedNetworkInput, ReceivedSnapshotAck, SendNetworkSnapshot},
    snapshot::{
        FullBody, MARGIN_UNMEASURED, RelayedInput, SnapshotBody, SnapshotPacket, build_full_body,
        encode_packet_with,
    },
};

/// Resource identifying the local player on the server (for listen-server setups).
#[derive(Resource)]
pub struct LocalServerPlayer(pub u128);

/// Who a snapshot goes to: every client the transport has verified, by uuid.
///
/// Maintained by the transport layer, which is the only thing that knows what a lobby is.
/// Each entry gets its own packet, with its own sequence number and its own input margin.
/// **Absent means "unknown, send to everyone"**: one unaddressed packet, which is how a world
/// with no transport (a test) behaves. Present and empty means nobody is listening, and
/// nothing is built — the test is before the encode, on purpose: a host with no clients used
/// to postcard the whole world sixty-four times a second and throw every byte away, which is
/// why solo play could not simply be "a host with no clients".
#[derive(Resource, Default, Debug, Clone)]
pub struct SnapshotRecipientList(pub Vec<u128>);

/// Per-recipient snapshot sequence numbers.
#[derive(Resource, Default, Debug, Clone)]
pub struct SnapshotSeq(pub HashMap<u128, u32>);

/// The newest snapshot `seq` each client has acknowledged. What a delta is built against.
#[derive(Resource, Default, Debug, Clone)]
pub struct LastAck(pub HashMap<u128, u32>);

/// Each client's input-arrival margin, in ticks, as reported to it in the next snapshot: the
/// *worst* of `input.tick - server_tick` over the first copies of its inputs that arrived since
/// the last snapshot sent to it. The client keeps the worst of these over the last second and
/// sizes its prediction lead from that (see [`ClientTickBuffer`](crate::client::ClientTickBuffer)).
///
/// The worst, and not the latest, because the lead exists for the unlucky input. It used to be
/// the latest input at the moment of sending — one sample per snapshot, the rest unseen — which
/// the client then averaged; so on a link with 40 ms of jitter it steered the *average* input to
/// two ticks early, and every input more than a tick later than that missed its tick: one in
/// eleven of them on congested Wi-Fi, each a snap of the player's own body.
///
/// First copies only: a redundant copy arrives later than the copy before it by construction,
/// and a straggler from a reordered packet says how late a packet was, not its input.
#[derive(Resource, Default, Debug)]
pub struct InputMargins(pub HashMap<u128, MeasuredMargin>);

/// One client's input-arrival margin and the tick it was measured at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MeasuredMargin {
    /// How many ticks ahead of the server the worst input since the last report arrived.
    /// `CurrentTick` is the tick the server last ran, so one or more is on time and zero or less
    /// is late.
    pub ticks: i64,
    /// The server tick at which the newest first copy arrived.
    pub at: Tick,
}

/// Clients whose [`InputMargins`] entry has gone out in a snapshot: the next input starts a new
/// worst rather than folding into the one already reported.
#[derive(Resource, Default, Debug)]
pub struct MarginsReported(pub std::collections::HashSet<u128>);

/// A margin older than this, in server ticks, is not reported. Inputs come every tick with
/// three ticks of redundancy, so a client that has sent nothing for a quarter of a second has
/// stopped sending — no body to drive, a menu up — and its last margin describes a lead it may
/// no longer hold. Reporting it as current let the client's target wander; reporting zero
/// instead was worse (see [`MARGIN_UNMEASURED`]).
pub const MARGIN_STALE_TICKS: Ticks = Ticks(16);

/// The input the host ran for each client on the tick it last ran.
///
/// Recorded after the tick rather than read back from the queue later, because by then the queue
/// holds what *should* have run: a late input is filed on its own tick when it arrives, after
/// that tick has gone. A press is a button held in a late input and not in this — see
/// [`LateInputFold`]. Kept only when the game registers one.
#[derive(Resource)]
pub struct InputsRan<T>(pub HashMap<u128, T>);

impl<T> core::fmt::Debug for InputsRan<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "InputsRan({} players)", self.0.len())
    }
}

impl<T> Default for InputsRan<T> {
    fn default() -> Self {
        Self(HashMap::new())
    }
}
impl<T: Send + Sync + 'static> SessionReset for InputsRan<T> {}
impl<T: Send + Sync + 'static> PerPeer for InputsRan<T> {
    fn forget(&mut self, peer: u128) {
        self.0.remove(&peer);
    }
}

/// PostTick: what each client's input was on the tick that just ran. See [`InputsRan`].
fn remember_inputs_ran<T: TickedInput>(
    tick: Res<CurrentTick>,
    queue: Res<InputQueue<T>>,
    newest: Res<NewestInputTick>,
    mut ran: ResMut<InputsRan<T>>,
) {
    for &sender in newest.0.keys() {
        if let Some(input) = queue.get_or_last(tick.0, sender) {
            ran.0.insert(sender, input.clone());
        }
    }
}

/// The input ticks each client has delivered, within the window the host keeps.
///
/// Every tick is sent several times over (see [`collect_network_inputs`]); this tells its first
/// copy, the one that says whether the input reached the host in time, from the redundant ones.
#[derive(Resource, Default, Debug)]
pub struct DeliveredInputTicks(pub HashMap<u128, BTreeSet<Tick>>);

/// The highest input tick seen from each client so far.
///
/// Only used to decide whether a late input may be forward-filled onto the next
/// tick — see [`collect_network_inputs`]. Kept per sender because "newest" is a
/// question about one client's stream, not about the session.
#[derive(Resource, Default, Debug)]
pub struct NewestInputTick(pub HashMap<u128, Tick>);

impl SessionReset for SnapshotSeq {}
impl PerPeer for SnapshotSeq {
    fn forget(&mut self, peer: u128) {
        self.0.remove(&peer);
    }
}
impl SessionReset for LastAck {}
impl PerPeer for LastAck {
    fn forget(&mut self, peer: u128) {
        self.0.remove(&peer);
    }
}
impl SessionReset for InputMargins {}
impl PerPeer for InputMargins {
    fn forget(&mut self, peer: u128) {
        self.0.remove(&peer);
    }
}
impl SessionReset for MarginsReported {}
impl PerPeer for MarginsReported {
    fn forget(&mut self, peer: u128) {
        self.0.remove(&peer);
    }
}
impl SessionReset for DeliveredInputTicks {}
impl PerPeer for DeliveredInputTicks {
    fn forget(&mut self, peer: u128) {
        self.0.remove(&peer);
    }
}
impl SessionReset for NewestInputTick {}
impl PerPeer for NewestInputTick {
    fn forget(&mut self, peer: u128) {
        self.0.remove(&peer);
    }
}
impl SessionReset for NackedFull {}
impl PerPeer for NackedFull {
    fn forget(&mut self, peer: u128) {
        self.0.remove(&peer);
    }
}
impl SessionReset for Baselines {}
impl PerPeer for Baselines {
    fn forget(&mut self, peer: u128) {
        Baselines::forget(self, peer);
    }
}

/// Plugin for the server side of multiplayer tick networking.
///
/// Hooks into `TickedPlugin`'s tick lifecycle:
/// - **PreTick**: collects inputs from `ReceivedNetworkInput<T>` into `InputQueue<T>`
/// - **PostTick**: broadcasts a `SendNetworkSnapshot` with the just-captured world state
///
/// The user must provide an input application system in `TickedSimulation`
/// that reads from `InputQueue<T>` and applies inputs to the game state.
pub struct TickedServerPlugin<T: TickedInput> {
    /// Broadcast a snapshot every this many ticks. `1` is every tick.
    ///
    /// A client that agrees with the authority costs nothing per snapshot now, so the rate is
    /// a bandwidth knob and no longer a smoothness one; a remote body is interpolated across
    /// the gap either way (the bridge sets `InterpolationDelay` to twice this).
    pub send_every: Ticks,
    /// Every this many packets to a client, a full body rather than a delta.
    pub keyframe_every: u32,
    /// How many sent packets to keep per client as candidate delta baselines. Must cover a
    /// round trip in packets, or a slow link gets keyframes only.
    pub max_unacked_baselines: usize,
    /// How packets are compressed on the wire.
    pub compression: Compression,
    /// Per-type delta send rates. Opt-in.
    pub send_rates: SendRates,
    /// Build deltas at all. Off is a full body on every packet.
    pub deltas: bool,
    _phantom: PhantomData<T>,
}

impl<T: TickedInput> TickedServerPlugin<T> {
    pub fn new() -> Self {
        Self {
            send_every: Ticks::ONE,
            keyframe_every: 64,
            max_unacked_baselines: 32,
            compression: Compression::default(),
            send_rates: SendRates::default(),
            deltas: true,
            _phantom: PhantomData,
        }
    }

    pub fn send_every(mut self, ticks: Ticks) -> Self {
        self.send_every = ticks.max(Ticks::ONE);
        self
    }

    pub fn keyframe_every(mut self, packets: u32) -> Self {
        self.keyframe_every = packets.max(1);
        self
    }

    pub fn compression(mut self, compression: Compression) -> Self {
        self.compression = compression;
        self
    }

    pub fn send_rates(mut self, rates: SendRates) -> Self {
        self.send_rates = rates;
        self
    }

    /// Full bodies only, never a delta.
    pub fn without_deltas(mut self) -> Self {
        self.deltas = false;
        self
    }
}

/// Runtime copy of [`TickedServerPlugin::compression`].
#[derive(Resource, Clone, Copy, Debug)]
pub struct SnapshotCompression(pub Compression);

/// Clients that asked for a full body.
#[derive(Resource, Default, Debug, Clone)]
pub struct NackedFull(pub std::collections::HashSet<u128>);

/// Runtime copy of [`TickedServerPlugin::send_every`].
#[derive(Resource, Debug, Clone, Copy, PartialEq, Eq)]
pub struct SendEvery(pub Ticks);

impl<T: TickedInput> Default for TickedServerPlugin<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: TickedInput> Plugin for TickedServerPlugin<T> {
    fn build(&self, app: &mut App) {
        crate::input::install_input_queue::<T>(app);
        crate::replication::install_owner(app);
        crate::pause::install(app);
        crate::session::install(app);
        crate::watchdog::install_server(app);
        app.insert_resource(SendEvery(self.send_every.max(Ticks::ONE)))
            .insert_resource(DeltaPolicy {
                keyframe_every: self.keyframe_every.max(1),
                max_unacked_baselines: self.max_unacked_baselines.max(1),
                enabled: self.deltas,
            })
            .insert_resource(SnapshotCompression(self.compression))
            .insert_resource(self.send_rates.clone())
            .init_resource::<InputStats>()
            .init_resource::<SnapshotStats>()
            // What a host holds about each client, and about its own broadcast. Per peer, so a
            // client that leaves a running session takes its entries with it; and reset at every
            // door, because every one of them is keyed by a tick or a `seq` of a clock that
            // restarts at zero — a high-water mark from the last session makes every input of the
            // next one look stale.
            .init_per_peer_resource::<Baselines>()
            .init_per_peer_resource::<NackedFull>()
            .init_per_peer_resource::<InputMargins>()
            .init_per_peer_resource::<NewestInputTick>()
            .init_per_peer_resource::<DeliveredInputTicks>()
            .init_per_peer_resource::<MarginsReported>()
            .init_per_peer_resource::<InputsRan<T>>()
            .init_per_peer_resource::<SnapshotSeq>()
            .init_per_peer_resource::<LastAck>()
            .init_session_resource_scoped::<PassesHeld>(SessionScope::Role)
            .add_observer(collect_network_inputs::<T>)
            .add_observer(record_ack)
            .add_systems(
                TickedLoop,
                (
                    broadcast_snapshot::<T>,
                    remember_inputs_ran::<T>.run_if(resource_exists::<LateInputFold<T>>),
                )
                    .in_set(TickedSystems::PostTick),
            );
    }

    fn finish(&self, app: &mut App) {
        bevy_ticked::require_steerable_tick_source(app, "TickedServerPlugin");
    }
}

/// Observer: collect incoming network inputs into the InputQueue.
///
/// # Late input is used, not discarded
///
/// The simulation reads `InputQueue::at_tick(current_tick)` and nothing else, so
/// an input stamped for a tick the server has already run lands where nothing
/// will ever read it. That is a silent, total loss of a keypress, and on a
/// jittery link it is a large fraction of them: a spike that pushes one packet
/// past its tick swallows whatever the player pressed, and the held-input
/// fallback most consumers use then carries on with the *previous* value — so a
/// direction change is not merely delayed, it is skipped, and the player feels
/// the character refuse to turn.
///
/// So a late input is also filed on the next tick the server will run. One tick
/// of staleness in place of a dropped one.
///
/// Only from the newest input that sender has produced, which is what
/// [`NewestInputTick`] is for. Each packet carries a few ticks of redundant
/// history and packets can arrive out of order, so without that guard a
/// straggler carrying an *older* input would overwrite the fresher one already
/// sitting on the next tick — turning a mechanism for recovering input into one
/// for corrupting it.
///
/// # The window
///
/// An input is accepted only if `server_tick - HistoryBufferTicks <= tick <=
/// server_tick + MAX_INPUT_LEAD_TICKS`. Anything else is counted in
/// [`InputStats::dropped_out_of_window`] and then ignored entirely: it does not
/// enter the queue, it does not move the sender's margin, and it does not raise
/// the sender's [`NewestInputTick`].
///
/// Before the window, one client — hostile or merely buggy — could put a tick at
/// `u64::MAX` into the queue, and from then on every snapshot carried a margin
/// of nine quintillion, the prune system never reached that tick, and a
/// forward-fill guard that trusted "newest" refused every input the client sent
/// afterwards. The lower bound is the same as the prune window because an older
/// tick is unreplayable anyway; the upper bound is the client's own lead ceiling,
/// so a legitimate client can never be refused.
fn collect_network_inputs<T: TickedInput>(
    trigger: On<ReceivedNetworkInput<T>>,
    tick: Res<CurrentTick>,
    window: Res<HistoryBufferTicks>,
    mut queue: ResMut<InputQueue<T>>,
    mut margins: ResMut<InputMargins>,
    mut newest: ResMut<NewestInputTick>,
    mut delivered: ResMut<DeliveredInputTicks>,
    mut reported: ResMut<MarginsReported>,
    fold: Option<Res<LateInputFold<T>>>,
    ran: Res<InputsRan<T>>,
    mut stats: ResMut<InputStats>,
    mut ack_watch: ResMut<crate::watchdog::AckWatch>,
) {
    let event = trigger.event();
    ack_watch.input_from(event.sender);
    let oldest_accepted = tick.0.saturating_sub(window.0);
    let newest_accepted = tick.0.saturating_add(MAX_INPUT_LEAD_TICKS);
    if event.tick < oldest_accepted || event.tick > newest_accepted {
        stats.dropped_out_of_window += 1;
        return;
    }
    // How many ticks ahead of the server this input arrived. Reported back to the client so it
    // can adapt its prediction lead. `CurrentTick` is the tick the host last *ran* — inputs are
    // read before this frame's ticks — so an input for it is already too late: on time is one or
    // more.
    let margin = event.tick.offset_from(tick.0);
    let ticks = delivered.0.entry(event.sender).or_default();
    if ticks.first().is_some_and(|&first| first < oldest_accepted) {
        *ticks = ticks.split_off(&oldest_accepted);
    }
    let first_copy = ticks.insert(event.tick);
    stats.received += 1;
    if margin <= 0 {
        stats.late += 1;
    }
    if first_copy {
        stats.new_ticks += 1;
        if margin <= 0 {
            stats.late_new_ticks += 1;
        }
        let fresh = reported.0.remove(&event.sender);
        let worst = match margins.0.get(&event.sender) {
            Some(measured) if !fresh => measured.ticks.min(margin),
            _ => margin,
        };
        margins.0.insert(
            event.sender,
            MeasuredMargin {
                ticks: worst,
                at: tick.0,
            },
        );
    }
    // What the next tick held for this sender before this input, and how late the input it
    // came from was: when it is a late input filed forward, that is the newest input the sender
    // had produced until now.
    let next_tick = tick.0.next();
    let held_next = fold
        .as_ref()
        .and_then(|_| queue.get(next_tick, event.sender).cloned());

    queue.insert(event.tick, event.sender, event.input.clone());

    let seen = newest.0.entry(event.sender).or_insert(Tick::ZERO);
    let previous_newest = *seen;
    let is_newest = event.tick > previous_newest;
    if is_newest {
        *seen = event.tick;
        if event.tick <= tick.0 {
            queue.insert(next_tick, event.sender, event.input.clone());
        }
    }

    if let Some(fold) = fold {
        let replaced_next = event.tick == next_tick || (is_newest && event.tick <= tick.0);
        let held_age = if previous_newest <= tick.0 {
            tick.0.since(previous_newest)
        } else {
            Ticks::ZERO
        };
        fold_late_input(
            &fold,
            &mut queue,
            ran.0.get(&event.sender),
            event,
            tick.0,
            first_copy && margin <= 0 && !is_newest,
            held_next.filter(|_| replaced_next && held_age <= fold.max_age),
        );
    }
}

/// Keep what a late input did, on the next tick the host runs. See [`LateInputFold`].
///
/// Two ways a late input's press is lost, and the two things done here:
/// - **Overtaken.** Its own release, or anything newer, replaced it on the next tick. What the
///   next tick held before is folded into what it holds now (`replaced`), so a press carried
///   there survives every later write — the release, a redundant copy, the input meant for that
///   tick.
/// - **Straggler.** It came after a newer input, so it was never filed forward at all. Folded
///   into what the next tick would run.
fn fold_late_input<T: TickedInput>(
    fold: &LateInputFold<T>,
    queue: &mut InputQueue<T>,
    ran: Option<&T>,
    event: &ReceivedNetworkInput<T>,
    server_tick: Tick,
    straggler: bool,
    replaced: Option<T>,
) {
    let next_tick = server_tick.next();
    // Nothing run for this sender yet: there is no press to tell from a hold.
    let Some(ran) = ran else {
        return;
    };
    if let Some(replaced) = replaced
        && let Some(mut next) = queue.get(next_tick, event.sender).cloned()
    {
        (fold.fold)(&mut next, &replaced, ran);
        queue.insert(next_tick, event.sender, next);
    }
    if straggler && server_tick.since(event.tick) <= fold.max_age {
        let Some(mut next) = queue.get_or_last(next_tick, event.sender).cloned() else {
            return;
        };
        (fold.fold)(&mut next, &event.input, ran);
        queue.insert(next_tick, event.sender, next);
    }
}

fn record_ack(
    trigger: On<ReceivedSnapshotAck>,
    mut acks: ResMut<LastAck>,
    mut nacked: ResMut<NackedFull>,
) {
    let ack = *trigger.event();
    let newest = acks.0.entry(ack.sender).or_insert(ack.seq);
    if ack.seq >= *newest {
        *newest = ack.seq;
    }
    if ack.nack_full {
        nacked.0.insert(ack.sender);
    }
}

/// After the core tick, build and broadcast a snapshot.
/// Only runs if `LocalServerPlayer` is present (i.e., this peer is the host).
///
/// A held clock does not stop the broadcast. A host that pauses still has clients to tell,
/// and a client that joins during the pause needs the world; what changes is the rate: the
/// tick is the same every pass, so once every [`HELD_BROADCAST_EVERY`] passes is enough for a
/// joiner and spares everyone else a stream of identical packets.
fn broadcast_snapshot<T: TickedInput>(
    tick: Res<CurrentTick>,
    holds: Res<TickHolds>,
    send_every: Res<SendEvery>,
    server_player: Option<Res<LocalServerPlayer>>,
    recipients: Option<Res<SnapshotRecipientList>>,
    mut passes_held: ResMut<PassesHeld>,
    mut commands: Commands,
) {
    if server_player.is_none() || holds.holds(TickHoldReason::AwaitingSync) {
        return;
    }
    if !tick.0.is_multiple_of(send_every.0.max(Ticks::ONE)) {
        return;
    }
    // The first held pass always sends, so a pause reaches every client the tick it starts;
    // after that, once every `HELD_BROADCAST_EVERY` passes.
    if holds.is_held() {
        if passes_held.0 > 0 && !passes_held.0.is_multiple_of(HELD_BROADCAST_EVERY) {
            passes_held.0 += 1;
            return;
        }
        passes_held.0 += 1;
    } else {
        passes_held.0 = 0;
    }
    // Before the encode, which is the whole point. See [`SnapshotRecipientList`].
    if recipients.is_some_and(|recipients| recipients.0.is_empty()) {
        return;
    }
    commands.queue(BroadcastSnapshotCommand::<T>(tick.0, PhantomData));
}

/// Consecutive loop passes the host's clock has been held for, which paces the held broadcast.
///
/// A resource registered with the session, where it used to be a `Local`: a host that left with
/// its clock held — a pause menu open, which is the game's hold and survives every door — and
/// hosted again still held had the first snapshot of the new session throttled by the count the
/// last one reached, and a joiner waited half a second longer for its world than on a fresh app.
#[derive(Resource, Default, Debug, Clone, Copy, PartialEq, Eq)]
pub struct PassesHeld(pub u32);

impl SessionReset for PassesHeld {}

/// Passes of the loop between snapshots while the clock is held: half a second at 64 Hz.
const HELD_BROADCAST_EVERY: u32 = 32;

struct BroadcastSnapshotCommand<T>(Tick, PhantomData<T>);

impl<T: TickedInput> Command for BroadcastSnapshotCommand<T> {
    type Out = ();

    fn apply(self, world: &mut World) {
        let tick = self.0;
        let mut body = build_full_body(world, tick);
        body.inputs_ahead = inputs_ahead::<T>(world, tick);

        let recipients: Option<Vec<u128>> = world
            .get_resource::<SnapshotRecipientList>()
            .map(|list| list.0.clone());
        let targets: Vec<Option<u128>> = match recipients {
            Some(list) => list.into_iter().map(Some).collect(),
            None => vec![None],
        };
        let registry = world.resource::<TickedComponentRegistry>().clone();
        let policy = *world.resource::<DeltaPolicy>();
        let compression = world.resource::<SnapshotCompression>().0;
        world.resource_mut::<SendRates>().resolve(&registry);
        let rates = world.resource::<SendRates>().clone();
        // Split once; every recipient's delta reads the same layout.
        let layouts = if policy.enabled {
            layouts_of(&registry, &body)
        } else {
            Vec::new()
        };
        for recipient in targets {
            let seq = match recipient {
                Some(uuid) => {
                    let mut seqs = world.resource_mut::<SnapshotSeq>();
                    let next = seqs.0.entry(uuid).or_insert(0);
                    *next = next.wrapping_add(1);
                    *next
                }
                None => 0,
            };
            let measured = recipient
                .and_then(|uuid| world.resource::<InputMargins>().0.get(&uuid).copied())
                .filter(|measured| tick.since(measured.at) <= MARGIN_STALE_TICKS);
            let your_margin = measured.map_or(MARGIN_UNMEASURED, |measured| {
                // One above the sentinel, so a genuinely enormous lateness stays a number.
                measured.ticks.clamp(i16::MIN as i64 + 1, i16::MAX as i64) as i16
            });
            if let (Some(uuid), Some(_)) = (recipient, measured) {
                world.resource_mut::<MarginsReported>().0.insert(uuid);
            }

            // Which ids have been handed to a different thing since the world this recipient
            // last acknowledged. Relative to their baseline, because a reset is not free: it
            // throws away whatever the game hung on the entity locally, so naming an id that did
            // not change hands re-dresses something that was already right.
            let baseline_tick = recipient.and_then(|uuid| {
                let acked = world.resource::<LastAck>().0.get(&uuid).copied()?;
                world
                    .resource::<Baselines>()
                    .get(uuid, acked)
                    .map(|baseline| baseline.tick)
            });
            let reborn: Vec<u64> = match world.get_resource::<TrackedEntityLifetimes>() {
                Some(lifetimes) => match baseline_tick {
                    Some(at) => lifetimes.reborn_since(at).collect(),
                    // No usable baseline, so a full body is going out. A joiner holds no ids for
                    // a rename to confuse; a client whose ack fell off the ring does, so it gets
                    // the whole window — a superset whose only cost is re-dressing an entity that
                    // was already right.
                    None => lifetimes.reborn_ids().collect(),
                },
                None => Vec::new(),
            };

            // Delta or keyframe. A delta only against a baseline the client acknowledged and
            // the ring still holds; a full body on a keyframe, after a nack, and to anyone
            // without a usable ack (a joiner, a client whose ack fell off the ring).
            let mut delta = None;
            if let Some(uuid) = recipient
                && policy.enabled
                && !seq.is_multiple_of(policy.keyframe_every)
                && !world.resource_mut::<NackedFull>().0.remove(&uuid)
                && let Some(acked) = world.resource::<LastAck>().0.get(&uuid).copied()
                && let Some(baseline) = world.resource::<Baselines>().get(uuid, acked)
            {
                delta = Some({
                    let mut built = build_delta(&registry, &body, &layouts, baseline, &rates, seq);
                    built.reborn = reborn.clone();
                    built
                });
            }
            let is_delta = delta.is_some();
            let packet = SnapshotPacket {
                seq,
                tick,
                your_margin,
                body: match delta {
                    Some(delta) => SnapshotBody::Delta(delta),
                    None => SnapshotBody::Full(FullBody {
                        reborn,
                        ..body.clone()
                    }),
                },
            };
            let bytes = encode_packet_with(&packet, compression);
            if let Some(mut stats) = world.get_resource_mut::<SnapshotStats>() {
                stats.sent += 1;
                stats.record_bytes(bytes.len());
                if is_delta {
                    stats.deltas += 1;
                } else {
                    stats.keyframes += 1;
                }
            }
            if let Some(uuid) = recipient
                && policy.enabled
            {
                world.resource_mut::<Baselines>().push(
                    uuid,
                    Baseline {
                        seq,
                        tick,
                        body: body.clone(),
                        layouts: layouts.clone(),
                    },
                    policy.max_unacked_baselines,
                );
            }
            world
                .commands()
                .trigger(SendNetworkSnapshot { recipient, bytes });
        }
    }
}

/// Every input the host holds for ticks after `tick`, for every player, encoded. A client
/// drops its own on arrival; the rest let its replay use what other players pressed.
fn inputs_ahead<T: TickedInput>(world: &World, tick: Tick) -> Vec<RelayedInput> {
    let queue = world.resource::<InputQueue<T>>();
    let mut out = Vec::new();
    for at in tick.next().through(tick + MAX_INPUT_LEAD_TICKS) {
        let Some(inputs) = queue.at_tick(at) else {
            continue;
        };
        for (player, input) in inputs {
            if let Ok(bytes) = postcard::to_allocvec(input) {
                out.push(RelayedInput {
                    player: *player,
                    tick: at,
                    bytes,
                });
            }
        }
    }
    out
}
