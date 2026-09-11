 use std::collections::HashMap;
use std::marker::PhantomData;

use bevy::prelude::*;

use bevy_ticked::{
    TickedLoop, TickedSystems,
    registry::TickedComponentRegistry,
    tick::{CurrentTick, HistoryBufferTicks, TickHoldReason, TickHolds},
    tracked_entity::{TickTrackedEntity, TickTrackedEntityCounter},
};

use crate::{
    diagnostics::{InputStats, SnapshotStats},
    input::{InputQueue, MAX_INPUT_LEAD_TICKS, TickedInput},
    messages::{PeerLeft, ReceivedNetworkInput, ReceivedSnapshotAck, SendNetworkSnapshot},
    snapshot::{RelayedInput, SnapshotBody, SnapshotPacket, build_full_body, encode_packet},
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

/// Latest input-arrival margin (in ticks) per client, measured by the server:
/// `input.tick - server_tick` at arrival. Sent to clients in each snapshot so they
/// can size their prediction lead from the real thing (see [`WorldSnapshot`]).
///
/// [`WorldSnapshot`]: crate::snapshot::WorldSnapshot
#[derive(Resource, Default)]
pub struct InputMargins(pub HashMap<u128, i64>);

/// The highest input tick seen from each client so far.
///
/// Only used to decide whether a late input may be forward-filled onto the next
/// tick — see [`collect_network_inputs`]. Kept per sender because "newest" is a
/// question about one client's stream, not about the session.
#[derive(Resource, Default)]
pub struct NewestInputTick(pub HashMap<u128, u64>);

/// Plugin for the server side of multiplayer tick networking.
///
/// Hooks into `TickedPlugin`'s tick lifecycle:
/// - **PreTick**: collects inputs from `ReceivedNetworkInput<T>` into `InputQueue<T>`
/// - **PostTick**: broadcasts a `SendNetworkSnapshot` with the just-captured world state
///
/// The user must provide an input application system in `TickedSimulation`
/// that reads from `InputQueue<T>` and applies inputs to the game state.
pub struct TickedServerPlugin<T: TickedInput> {
    _phantom: PhantomData<T>,
}

impl<T: TickedInput> TickedServerPlugin<T> {
    pub fn new() -> Self {
        Self {
            _phantom: PhantomData,
        }
    }
}

impl<T: TickedInput> Default for TickedServerPlugin<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: TickedInput> Plugin for TickedServerPlugin<T> {
    fn build(&self, app: &mut App) {
        crate::input::install_input_queue::<T>(app);
        crate::replication::install_owner(app);
        app.init_resource::<InputMargins>()
            .init_resource::<NewestInputTick>()
            .init_resource::<InputStats>()
            .init_resource::<SnapshotStats>()
            .init_resource::<SnapshotSeq>()
            .init_resource::<LastAck>()
            .add_observer(collect_network_inputs::<T>)
            .add_observer(record_ack)
            .add_observer(forget_departed_peer::<T>)
            .add_systems(
                Update,
                reset_on_host::<T>.run_if(resource_added::<LocalServerPlayer>),
            )
            .add_systems(
                Update,
                crate::reset_on_leave::<T>.run_if(resource_removed::<LocalServerPlayer>),
            )
            .add_systems(
                TickedLoop,
                broadcast_snapshot::<T>.in_set(TickedSystems::PostTick),
            );
    }

    fn finish(&self, app: &mut App) {
        bevy_ticked::require_steerable_tick_source(app, "TickedServerPlugin");
    }
}

/// When `LocalServerPlayer` is inserted, reset tick state so the
/// multiplayer session starts fresh from tick 0.
///
/// The counter is set to the world's **high-water mark**, not to zero, and that is
/// the whole point of this function's shape. Zeroing it while tracked entities are
/// still standing hands the next `next()` an id that is already in use, and
/// `apply_snapshot` keys the entire world by id — so a rope that collides with a
/// player has its components merged onto that player and no rope is ever created.
/// The host sees a rope; the joiner watches the shot freeze and nothing appear.
///
/// Despawning instead would also close the hole, but it is the wrong trade here: a
/// solo player opening their world to friends would lose it. A client has no such
/// claim, which is why [`reset_on_join`] does despawn.
///
/// The invariant either way: **no id is ever issued twice in a session.**
fn reset_on_host<T: TickedInput>(world: &mut World) {
    let highest = highest_tracked_id(world);
    world.insert_resource(CurrentTick(0));
    // A host's clock is the session's clock: nothing to wait for. Its own reasons only; a game
    // that opened the lobby from a pause menu keeps its pause.
    let mut holds = world.resource_mut::<TickHolds>();
    holds.release(TickHoldReason::AwaitingSync);
    holds.release(TickHoldReason::SoftHold);
    world.insert_resource(TickTrackedEntityCounter(highest));
    world.resource_mut::<InputQueue<T>>().inputs.clear();
    // The tick counter goes back to zero, so a high-water mark from the last
    // session would make every input of this one look stale.
    world.insert_resource(NewestInputTick::default());
    world.insert_resource(InputMargins::default());
    world.insert_resource(SnapshotSeq::default());
    world.insert_resource(LastAck::default());
    let registry = world.resource::<TickedComponentRegistry>().clone();
    registry.clear_all(world);
}

/// The largest `TickTrackedEntity` id currently in the world, or 0 if there are none.
pub(crate) fn highest_tracked_id(world: &mut World) -> u64 {
    let mut tracked = world.query::<&TickTrackedEntity>();
    tracked.iter(world).map(|tracked| tracked.0).max().unwrap_or(0)
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
    mut stats: ResMut<InputStats>,
) {
    let event = trigger.event();
    let oldest_accepted = tick.0.saturating_sub(window.0);
    let newest_accepted = tick.0.saturating_add(MAX_INPUT_LEAD_TICKS);
    if event.tick < oldest_accepted || event.tick > newest_accepted {
        stats.dropped_out_of_window += 1;
        return;
    }
    // How many ticks ahead of the server this input arrived (negative = late).
    // Reported back to the client so it can adapt its prediction lead.
    let margin = event.tick as i64 - tick.0 as i64;
    stats.received += 1;
    if margin < 0 {
        stats.late += 1;
    }
    margins.0.insert(event.sender, margin);
    queue.insert(event.tick, event.sender, event.input.clone());

    let seen = newest.0.entry(event.sender).or_insert(0);
    if event.tick <= *seen {
        return;
    }
    *seen = event.tick;
    if event.tick <= tick.0 {
        queue.insert(tick.0 + 1, event.sender, event.input.clone());
    }
}

/// Observer: a client left, so nothing the host holds per sender may outlive it.
///
/// Its inputs at every tick (or the body it left behind keeps obeying its last
/// keypress until the window prunes it), its margin (or every snapshot keeps
/// reporting a player who is not there), and its newest-tick mark (or a rejoin
/// under the same uuid finds all of its inputs older than "newest" and never
/// gets one forward-filled). See [`PeerLeft`].
fn forget_departed_peer<T: TickedInput>(
    trigger: On<PeerLeft>,
    mut queue: ResMut<InputQueue<T>>,
    mut margins: ResMut<InputMargins>,
    mut newest: ResMut<NewestInputTick>,
) {
    let uuid = trigger.event().0;
    queue.remove_player(uuid);
    margins.0.remove(&uuid);
    newest.0.remove(&uuid);
}

fn record_ack(trigger: On<ReceivedSnapshotAck>, mut acks: ResMut<LastAck>) {
    let ack = *trigger.event();
    let newest = acks.0.entry(ack.sender).or_insert(ack.seq);
    if ack.seq >= *newest {
        *newest = ack.seq;
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
    server_player: Option<Res<LocalServerPlayer>>,
    recipients: Option<Res<SnapshotRecipientList>>,
    mut passes_held: Local<u32>,
    mut commands: Commands,
) {
    if server_player.is_none() || holds.holds(TickHoldReason::AwaitingSync) {
        return;
    }
    if holds.is_held() {
        *passes_held += 1;
        if !(*passes_held).is_multiple_of(HELD_BROADCAST_EVERY) {
            return;
        }
    } else {
        *passes_held = 0;
    }
    // Before the encode, which is the whole point. See [`SnapshotRecipientList`].
    if recipients.is_some_and(|recipients| recipients.0.is_empty()) {
        return;
    }
    commands.queue(BroadcastSnapshotCommand::<T>(tick.0, PhantomData));
}

/// Passes of the loop between snapshots while the clock is held: half a second at 64 Hz.
const HELD_BROADCAST_EVERY: u32 = 32;

struct BroadcastSnapshotCommand<T>(u64, PhantomData<T>);

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
            let your_margin = recipient
                .and_then(|uuid| world.resource::<InputMargins>().0.get(&uuid).copied())
                .map_or(0, |margin| margin.clamp(i16::MIN as i64, i16::MAX as i64) as i16);
            let packet = SnapshotPacket {
                seq,
                tick,
                your_margin,
                body: SnapshotBody::Full(body.clone()),
            };
            let bytes = encode_packet(&packet);
            if let Some(mut stats) = world.get_resource_mut::<SnapshotStats>() {
                stats.sent += 1;
                stats.record_bytes(bytes.len());
            }
            world.commands().trigger(SendNetworkSnapshot { recipient, bytes });
        }
    }
}

/// Every input the host holds for ticks after `tick`, for every player, encoded. A client
/// drops its own on arrival; the rest let its replay use what other players pressed.
fn inputs_ahead<T: TickedInput>(world: &World, tick: u64) -> Vec<RelayedInput> {
    let queue = world.resource::<InputQueue<T>>();
    let mut out = Vec::new();
    for at in (tick + 1)..=(tick + MAX_INPUT_LEAD_TICKS) {
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
