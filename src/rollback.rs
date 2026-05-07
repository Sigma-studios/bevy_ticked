use bevy::ecs::schedule::ScheduleLabel;
use bevy::prelude::*;

use crate::{registry::TickedComponentRegistry, tick::CurrentTick};

/// Restore world state to a previous tick from WorldActions history.
///
/// Sets `CurrentTick` to `target_tick` and overwrites all registered components
/// on entities with their saved state at that tick.
pub fn rollback_to_tick(world: &mut World, target_tick: u64) {
    let registry = world.resource::<TickedComponentRegistry>().clone();
    registry.restore_all(world, target_tick);
    world.resource_mut::<CurrentTick>().0 = target_tick;
}

/// Rollback to `target_tick`, then re-simulate forward to `end_tick`.
///
/// For each tick between `target_tick + 1` and `end_tick` (inclusive):
/// 1. Sets `CurrentTick`
/// 2. Runs the `TickedSimulation` schedule
/// 3. Captures state into WorldActions
pub fn rollback_and_resimulate(
    world: &mut World,
    target_tick: u64,
    end_tick: u64,
    simulation_schedule: impl ScheduleLabel,
) {
    let registry = world.resource::<TickedComponentRegistry>().clone();

    // Restore state at target_tick
    registry.restore_all(world, target_tick);
    world.resource_mut::<CurrentTick>().0 = target_tick;

    // Truncate any history after target_tick (it's now invalid)
    registry.truncate_all_after(world, target_tick);

    // Re-simulate forward
    for tick in (target_tick + 1)..=end_tick {
        world.resource_mut::<CurrentTick>().0 = tick;
        world.run_schedule(simulation_schedule.intern());
        registry.capture_all(world, tick);
    }
}
