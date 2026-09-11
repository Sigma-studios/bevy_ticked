 use std::collections::HashMap;
use std::marker::PhantomData;

use bevy::prelude::*;

use bevy_ticked::{
    TickedLoop, TickedSystems,
    registry::TickedComponentRegistry,
    tick::{CurrentTick, TicksPaused},
    tracked_entity::{TickTrackedEntity, TickTrackedEntityCounter},
};

use crate::{
    diagnostics::{InputStats, SnapshotStats},
    input::{InputQueue, TickedInput},
    messages::{ReceivedNetworkInput, SendNetworkSnapshot},
    snapshot::build_snapshot,
};

/// Resource identifying the local player on the server (for listen-server setups).
#[derive(Resource)]
pub struct LocalServerPlayer(pub u128);

/// How many peers a snapshot built right now would actually reach.
///
/// # Why this is a resource and not a query
///
/// `broadcast_snapshot` gated on `LocalServerPlayer` and nothing else, and
/// `BroadcastSnapshotCommand` serialised the entire world *before* the transport discovered there
/// was nobody to send it to. The recipient test was on the far side of the expensive part.
///
/// That is not a small waste, it is a design constraint: it is the reason a peer playing alone
/// cannot simply insert `LocalServerPlayer` and be a host with no clients. A solo player who did
/// would postcard the whole world sixty-four times a second and throw every byte away — so solo
/// play has to hold *neither* role resource, and every consumer with a single-player mode then
/// needs its own three-valued idea of who it is, because upstream's is two booleans that are both
/// false. Both games that have a solo mode wrote that enum.
///
/// This crate cannot ask "is anyone listening" itself — it has no idea what a lobby is. So the
/// transport layer answers, here, before anything is serialised.
///
/// **Absent means "unknown, send anyway".** A transport that does not maintain this behaves
/// exactly as before, which is what makes adding it safe.
#[derive(Resource, Default, Debug, Clone, Copy)]
pub struct SnapshotRecipients(pub usize);

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
        app.init_resource::<InputMargins>()
            .init_resource::<NewestInputTick>()
            .init_resource::<InputStats>()
            .init_resource::<SnapshotStats>()
            .add_observer(collect_network_inputs::<T>)
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
                broadcast_snapshot.in_set(TickedSystems::PostTick),
            );
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
    world.insert_resource(TickTrackedEntityCounter(highest));
    world.resource_mut::<InputQueue<T>>().inputs.clear();
    // The tick counter goes back to zero, so a high-water mark from the last
    // session would make every input of this one look stale.
    world.insert_resource(NewestInputTick::default());
    world.insert_resource(InputMargins::default());
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
fn collect_network_inputs<T: TickedInput>(
    trigger: On<ReceivedNetworkInput<T>>,
    tick: Res<CurrentTick>,
    mut queue: ResMut<InputQueue<T>>,
    mut margins: ResMut<InputMargins>,
    mut newest: ResMut<NewestInputTick>,
    mut stats: ResMut<InputStats>,
) {
    let event = trigger.event();
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

/// After the core tick, build and broadcast a snapshot.
/// Only runs if `LocalServerPlayer` is present (i.e., this peer is the host).
fn broadcast_snapshot(
    tick: Res<CurrentTick>,
    ticks_paused: Option<Res<TicksPaused>>,
    server_player: Option<Res<LocalServerPlayer>>,
    recipients: Option<Res<SnapshotRecipients>>,
    mut commands: Commands,
) {
    if ticks_paused.is_some() || server_player.is_none() {
        return;
    }
    // Before `build_snapshot`, which is the whole point. See [`SnapshotRecipients`].
    if recipients.is_some_and(|recipients| recipients.0 == 0) {
        return;
    }
    commands.queue(BroadcastSnapshotCommand(tick.0));
}

struct BroadcastSnapshotCommand(u64);

impl Command for BroadcastSnapshotCommand {
    type Out = ();

    fn apply(self, world: &mut World) {
        let mut snapshot = build_snapshot(world, self.0);
        if let Some(margins) = world.get_resource::<InputMargins>() {
            snapshot.input_margins = margins.0.clone();
        }
        if let Some(mut stats) = world.get_resource_mut::<SnapshotStats>() {
            stats.sent += 1;
        }
        world.commands().trigger(SendNetworkSnapshot(snapshot));
    }
}
