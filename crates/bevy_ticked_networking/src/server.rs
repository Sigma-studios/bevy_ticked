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
    input::{InputQueue, TickedInput},
    messages::{ReceivedNetworkInput, SendNetworkSnapshot},
    snapshot::{build_snapshot, SnapshotBaseline, SnapshotSendRates},
};

/// Resource identifying the local player on the server (for listen-server setups).
#[derive(Resource)]
pub struct LocalServerPlayer(pub u128);

/// Latest input-arrival margin (in ticks) per client, measured by the server:
/// `input.tick - server_tick` at arrival. Sent to clients in each snapshot so they
/// can size their prediction lead from the real thing (see [`WorldSnapshot`]).
///
/// [`WorldSnapshot`]: crate::snapshot::WorldSnapshot
#[derive(Resource, Default)]
pub struct InputMargins(pub HashMap<u128, i64>);

/// How often the server puts a snapshot on the wire, and how much of one.
///
/// Both knobs default to "exactly what this crate has always done", so adding them changes no
/// existing behaviour. They are worth understanding before turning either up, because they trade
/// against different things.
#[derive(Resource, Clone, Copy, Debug)]
pub struct SnapshotPolicy {
    /// Send a snapshot every *N* ticks. 1 sends one per tick.
    ///
    /// Divides bandwidth by *N* exactly, and — because a client only rolls back when a snapshot
    /// lands — divides how *often* it replays by *N* as well. What it does not divide is total
    /// replay cost: each replay covers a longer stretch, so the saving is real but sublinear.
    /// What it does cost is correction latency, which becomes up to *N* ticks.
    pub send_every: u64,
    /// Send a complete snapshot every *N* sends; the ones between carry only what changed. 1
    /// makes every snapshot complete, which is the default and disables delta encoding.
    ///
    /// The reason this is off unless asked for: snapshots are unreliable and carry no sequence
    /// number, so a client that drops a delta is wrong about everything in it until the next
    /// keyframe — up to *N* sends later. At 1 a dropped snapshot costs a single tick of staleness,
    /// which is the property the rest of this crate is written against.
    pub keyframe_every: u64,
}

impl Default for SnapshotPolicy {
    fn default() -> Self {
        Self {
            send_every: 1,
            keyframe_every: 1,
        }
    }
}

impl SnapshotPolicy {
    fn sends_on(&self, tick: u64) -> bool {
        self.send_every <= 1 || tick % self.send_every == 0
    }

    fn keyframe_on(&self, tick: u64) -> bool {
        if self.keyframe_every <= 1 {
            return true;
        }
        let send_index = tick / self.send_every.max(1);
        send_index % self.keyframe_every == 0
    }
}

/// Set to force the next snapshot to be a complete one.
///
/// A client that has never seen a keyframe cannot decode a delta, so anything that adds a
/// recipient mid-stream has to ask for one. The server crate has no idea what a lobby is, so this
/// is the seam: the transport layer, or the game, raises it when somebody joins.
#[derive(Resource, Default)]
pub struct ForceKeyframe(pub bool);

/// Plugin for the server side of multiplayer tick networking.
///
/// Hooks into `TickedPlugin`'s tick lifecycle:
/// - **PreTick**: collects inputs from `ReceivedNetworkInput<T>` into `InputQueue<T>`
/// - **PostTick**: broadcasts a `SendNetworkSnapshot` with the just-captured world state
///
/// The user must provide an input application system in `TickedSimulation`
/// that reads from `InputQueue<T>` and applies inputs to the game state.
pub struct TickedServerPlugin<T: TickedInput> {
    /// See [`SnapshotPolicy`].
    pub policy: SnapshotPolicy,
    _phantom: PhantomData<T>,
}

impl<T: TickedInput> TickedServerPlugin<T> {
    pub fn new() -> Self {
        Self {
            policy: SnapshotPolicy::default(),
            _phantom: PhantomData,
        }
    }

    /// Send a snapshot every `every` ticks rather than every tick.
    pub fn send_every(mut self, every: u64) -> Self {
        self.policy.send_every = every.max(1);
        self
    }

    /// Make every `every`-th snapshot complete and delta-encode the rest.
    ///
    /// Read [`SnapshotPolicy::keyframe_every`] before raising this: it trades bandwidth for how
    /// long a dropped datagram stays wrong.
    pub fn keyframe_every(mut self, every: u64) -> Self {
        self.policy.keyframe_every = every.max(1);
        self
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
            .init_resource::<SnapshotBaseline>()
            .init_resource::<SnapshotSendRates>()
            .init_resource::<ForceKeyframe>()
            .insert_resource(self.policy)
            .add_observer(collect_network_inputs::<T>)
            .add_systems(
                Update,
                reset_on_host::<T>.run_if(resource_added::<LocalServerPlayer>),
            )
            .add_systems(
                TickedLoop,
                broadcast_snapshot.in_set(TickedSystems::PostTick),
            );
    }
}

/// Give a component type its own send rate. See [`SnapshotSendRates`].
///
/// Call after the type is registered; the index it is keyed by is assigned at registration.
pub trait SnapshotSendRateAppExt {
    fn set_snapshot_send_rate<T: bevy_ticked::registry::TickedComponent>(
        &mut self,
        every: u64,
    ) -> &mut Self;
}

impl SnapshotSendRateAppExt for App {
    fn set_snapshot_send_rate<T: bevy_ticked::registry::TickedComponent>(
        &mut self,
        every: u64,
    ) -> &mut Self {
        let index = self
            .world()
            .resource::<TickedComponentRegistry>()
            .index_of::<T>()
            .unwrap_or_else(|| {
                panic!(
                    "set_snapshot_send_rate::<{}>() before registering it",
                    std::any::type_name::<T>()
                )
            });
        self.init_resource::<SnapshotSendRates>()
            .world_mut()
            .resource_mut::<SnapshotSendRates>()
            .set(index, every);
        self
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
    let registry = world.resource::<TickedComponentRegistry>().clone();
    registry.clear_all(world);
}

/// The largest `TickTrackedEntity` id currently in the world, or 0 if there are none.
pub(crate) fn highest_tracked_id(world: &mut World) -> u64 {
    let mut tracked = world.query::<&TickTrackedEntity>();
    tracked.iter(world).map(|tracked| tracked.0).max().unwrap_or(0)
}

/// Observer: collect incoming network inputs into the InputQueue.
fn collect_network_inputs<T: TickedInput>(
    trigger: On<ReceivedNetworkInput<T>>,
    tick: Res<CurrentTick>,
    mut queue: ResMut<InputQueue<T>>,
    mut margins: ResMut<InputMargins>,
) {
    let event = trigger.event();
    // How many ticks ahead of the server this input arrived (negative = late).
    // Reported back to the client so it can adapt its prediction lead.
    let margin = event.tick as i64 - tick.0 as i64;
    margins.0.insert(event.sender, margin);
    queue.insert(event.tick, event.sender, event.input.clone());
}

/// After the core tick, build and broadcast a snapshot.
/// Only runs if `LocalServerPlayer` is present (i.e., this peer is the host).
fn broadcast_snapshot(
    tick: Res<CurrentTick>,
    ticks_paused: Option<Res<TicksPaused>>,
    server_player: Option<Res<LocalServerPlayer>>,
    policy: Res<SnapshotPolicy>,
    mut commands: Commands,
) {
    if ticks_paused.is_some() || server_player.is_none() || !policy.sends_on(tick.0) {
        return;
    }
    commands.queue(BroadcastSnapshotCommand(tick.0));
}

struct BroadcastSnapshotCommand(u64);

impl Command for BroadcastSnapshotCommand {
    type Out = ();

    fn apply(self, world: &mut World) {
        let policy = world.get_resource::<SnapshotPolicy>().copied().unwrap_or_default();
        let forced = world
            .get_resource_mut::<ForceKeyframe>()
            .map(|mut force| std::mem::replace(&mut force.0, false))
            .unwrap_or(false);
        let keyframe = forced || policy.keyframe_on(self.0);

        let full = build_snapshot(world, self.0);
        let rates = world.get_resource::<SnapshotSendRates>().is_some();

        let mut snapshot = if keyframe {
            if let Some(mut baseline) = world.get_resource_mut::<SnapshotBaseline>() {
                baseline.prime(&full);
            }
            full
        } else if rates {
            // Both resources are needed at once and both live in the world, so take the baseline
            // out rather than borrowing the world twice.
            let mut baseline = world.remove_resource::<SnapshotBaseline>().unwrap_or_default();
            let reduced = {
                let rates = world.resource::<SnapshotSendRates>();
                baseline.reduce(&full, rates)
            };
            world.insert_resource(baseline);
            reduced
        } else {
            full
        };

        if let Some(margins) = world.get_resource::<InputMargins>() {
            snapshot.input_margins = margins.0.clone();
        }
        world.commands().trigger(SendNetworkSnapshot(snapshot));
    }
}
