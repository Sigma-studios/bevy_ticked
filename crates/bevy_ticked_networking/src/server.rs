 use std::collections::HashMap;
use std::marker::PhantomData;

use bevy::prelude::*;

use bevy_ticked::{
    TickedSet,
    registry::TickedComponentRegistry,
    tick::{CurrentTick, TicksPaused},
    tracked_entity::TickTrackedEntityCounter,
};

use crate::{
    input::{InputQueue, TickedInput},
    messages::{ReceivedNetworkInput, SendNetworkSnapshot},
    snapshot::build_snapshot,
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
        app.init_resource::<InputQueue<T>>()
            .init_resource::<InputMargins>()
            .add_observer(collect_network_inputs::<T>)
            .add_systems(
                Update,
                reset_on_host::<T>.run_if(resource_added::<LocalServerPlayer>),
            )
            .add_systems(
                FixedUpdate,
                broadcast_snapshot.in_set(TickedSet::PostTick),
            );
    }
}

/// When `LocalServerPlayer` is inserted, reset tick state so the
/// multiplayer session starts fresh from tick 0.
fn reset_on_host<T: TickedInput>(world: &mut World) {
    world.insert_resource(CurrentTick(0));
    world.insert_resource(TickTrackedEntityCounter::default());
    world.resource_mut::<InputQueue<T>>().inputs.clear();
    let registry = world.resource::<TickedComponentRegistry>().clone();
    registry.clear_all(world);
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
    mut commands: Commands,
) {
    if ticks_paused.is_some() || server_player.is_none() {
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
        world.commands().trigger(SendNetworkSnapshot(snapshot));
    }
}
