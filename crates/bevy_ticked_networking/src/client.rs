use std::marker::PhantomData;

use bevy::prelude::*;

use bevy_ticked::{
    TickedSet, TickedSimulation,
    registry::TickedComponentRegistry,
    tick::{CurrentTick, TickConfig},
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

/// How many ticks ahead of the server the client should run.
///
/// This buffer ensures that client inputs arrive at the server before the
/// server reaches the tick they're intended for. Should be roughly RTT/2
/// in ticks. At 64hz: 6 ticks ≈ 94ms.
const CLIENT_TICK_BUFFER: u64 = 6;

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
    world.insert_resource(TickConfig { paused: true });
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

    let was_paused = world.resource::<TickConfig>().paused;
    let current_tick = world.resource::<CurrentTick>().0;
    let snapshot_tick = pending.snapshot.tick;
    let tick_buffer = CLIENT_TICK_BUFFER;

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
            world.resource_mut::<TickConfig>().paused = false;
        }
        return;
    }

    // If paused (shouldn't normally happen after initial sync), don't replay
    if was_paused {
        registry.capture_all(world, snapshot_tick);
        world.resource_mut::<TickConfig>().paused = false;
        return;
    }

    // Snapshot is behind us — rollback and replay predicted ticks
    registry.truncate_all_after(world, snapshot_tick);

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
