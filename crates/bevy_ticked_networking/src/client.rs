use std::marker::PhantomData;

use bevy::prelude::*;

use bevy_ticked::{
    TickedSet, TickedSimulation,
    registry::TickedComponentRegistry,
    tick::{CurrentTick, TickConfig},
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
            .add_observer(receive_snapshot)
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

/// PreTick: if a server snapshot arrived, rollback and replay local inputs to now.
fn handle_server_snapshot<T: TickedInput>(world: &mut World) {
    if world.resource::<TickConfig>().paused {
        return;
    }

    let Some(pending) = world.remove_resource::<PendingSnapshot>() else {
        return;
    };

    let current_tick = world.resource::<CurrentTick>().0;
    let snapshot_tick = pending.snapshot.tick;

    if snapshot_tick > current_tick {
        return;
    }

    let registry = world.resource::<TickedComponentRegistry>().clone();

    // Apply the authoritative snapshot (sets CurrentTick to snapshot_tick)
    apply_snapshot(world, &pending.snapshot);

    // Truncate any predicted state after the snapshot tick
    registry.truncate_all_after(world, snapshot_tick);

    // Replay forward from snapshot_tick+1 to current_tick
    for tick in (snapshot_tick + 1)..=current_tick {
        world.resource_mut::<CurrentTick>().0 = tick;
        world.run_schedule(TickedSimulation);
        registry.capture_all(world, tick);
    }
}

/// PostTick: send the local player's input for the current tick to the server.
fn send_local_input<T: TickedInput>(
    tick: Res<CurrentTick>,
    tick_config: Res<TickConfig>,
    local_player: Option<Res<LocalClientPlayer>>,
    queue: Res<InputQueue<T>>,
    mut commands: Commands,
) {
    if tick_config.paused {
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
