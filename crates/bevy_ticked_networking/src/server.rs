 use std::marker::PhantomData;

use bevy::prelude::*;

use bevy_ticked::{
    TickedSet,
    tick::{CurrentTick, TickConfig},
};

use crate::{
    input::{InputQueue, TickedInput},
    messages::{ReceivedNetworkInput, SendNetworkSnapshot},
    snapshot::build_snapshot,
};

/// Resource identifying the local player on the server (for listen-server setups).
#[derive(Resource)]
pub struct LocalServerPlayer(pub u128);

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
            .add_observer(collect_network_inputs::<T>)
            .add_systems(
                FixedUpdate,
                broadcast_snapshot.in_set(TickedSet::PostTick),
            );
    }
}

/// Observer: collect incoming network inputs into the InputQueue.
fn collect_network_inputs<T: TickedInput>(
    trigger: On<ReceivedNetworkInput<T>>,
    tick: Res<CurrentTick>,
    mut queue: ResMut<InputQueue<T>>,
) {
    let event = trigger.event();
    if event.tick < tick.0 {
        warn!(
            "Input from player {} arrived in the past (input tick: {}, server tick: {}, delta: {})",
            event.sender, event.tick, tick.0, tick.0 - event.tick
        );
    }
    queue.insert(event.tick, event.sender, event.input.clone());
}

/// After the core tick, build and broadcast a snapshot.
/// Only runs if `LocalServerPlayer` is present (i.e., this peer is the host).
fn broadcast_snapshot(
    tick: Res<CurrentTick>,
    tick_config: Res<TickConfig>,
    server_player: Option<Res<LocalServerPlayer>>,
    mut commands: Commands,
) {
    if tick_config.paused || server_player.is_none() {
        return;
    }
    commands.queue(BroadcastSnapshotCommand(tick.0));
}

struct BroadcastSnapshotCommand(u64);

impl Command for BroadcastSnapshotCommand {
    fn apply(self, world: &mut World) {
        let snapshot = build_snapshot(world, self.0);
        world.commands().trigger(SendNetworkSnapshot(snapshot));
    }
}
