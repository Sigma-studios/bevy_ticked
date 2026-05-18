pub mod prelude;
pub mod registry;
pub mod rollback;
pub mod tick;
pub mod tracked_entity;
pub mod world_actions;

use bevy::ecs::schedule::ScheduleLabel;
use bevy::prelude::*;

use registry::TickedComponentRegistry;
use rollback::rollback_and_resimulate;
use tick::{CurrentTick, ResetToTick, StepBackward, StepForward, TicksPaused};
use tracked_entity::TickTrackedEntityCounter;

/// The schedule where all tick-driven simulation systems run.
///
/// This schedule is independent from Bevy's built-in schedules. When unpaused,
/// it is driven by `FixedUpdate`. During rollback, it is run manually in a loop.
///
/// Add your game simulation systems to this schedule:
/// ```rust,ignore
/// app.add_systems(TickedSimulation, my_game_system);
/// ```
#[derive(ScheduleLabel, Clone, Debug, PartialEq, Eq, Hash)]
pub struct TickedSimulation;

/// System sets for ordering relative to tick advancement in `FixedUpdate`.
#[derive(SystemSet, Clone, Debug, PartialEq, Eq, Hash)]
pub enum TickedSet {
    /// Runs before tick advancement (e.g. client rollback on snapshot).
    PreTick,
    /// The core tick advancement: increment, run TickedSimulation, capture.
    Tick,
    /// Runs after tick advancement (e.g. server snapshot broadcast, client input send).
    PostTick,
}

pub struct TickedPlugin;

impl Plugin for TickedPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<CurrentTick>()
            .init_resource::<TickedComponentRegistry>()
            .init_resource::<TickTrackedEntityCounter>()
            .init_schedule(TickedSimulation)
            .add_message::<StepForward>()
            .add_message::<StepBackward>()
            .add_message::<ResetToTick>()
            .configure_sets(
                FixedUpdate,
                (TickedSet::PreTick, TickedSet::Tick, TickedSet::PostTick).chain(),
            )
            .add_systems(FixedUpdate, advance_tick_system.in_set(TickedSet::Tick))
            .add_systems(PreUpdate, apply_manual_controls);
    }
}

/// Capture the initial world state at tick 0 on the first run, then advance ticks when unpaused.
fn advance_tick_system(world: &mut World) {
    // On the very first run, capture the initial state at tick 0 so reset works.
    let current_tick = world.resource::<CurrentTick>().0;
    let registry = world.resource::<TickedComponentRegistry>().clone();
    if current_tick == 0 && !registry.is_empty() {
        let needs_capture = !registry.has_tick_captured(world, 0);
        if needs_capture {
            registry.capture_all(world, 0);
        }
    }

    if world.get_resource::<TicksPaused>().is_some() {
        return;
    }

    let tick = {
        let mut current = world.resource_mut::<CurrentTick>();
        current.0 += 1;
        current.0
    };

    world.run_schedule(TickedSimulation);

    let registry = world.resource::<TickedComponentRegistry>().clone();
    registry.capture_all(world, tick);
}

enum ManualControlAction {
    StepForward,
    StepBackward,
    Reset(u64),
}

/// Handle manual step/reset messages (works while paused). Exclusive system for World access.
fn apply_manual_controls(world: &mut World) {
    let mut actions = Vec::new();

    for _ in world.resource_mut::<Messages<StepForward>>().drain() {
        actions.push(ManualControlAction::StepForward);
    }
    for _ in world.resource_mut::<Messages<StepBackward>>().drain() {
        actions.push(ManualControlAction::StepBackward);
    }
    for reset in world
        .resource_mut::<Messages<ResetToTick>>()
        .drain()
        .collect::<Vec<_>>()
    {
        actions.push(ManualControlAction::Reset(reset.0));
    }

    for action in actions {
        match action {
            ManualControlAction::StepForward => {
                let tick = {
                    let mut current = world.resource_mut::<CurrentTick>();
                    current.0 += 1;
                    current.0
                };
                world.run_schedule(TickedSimulation);
                let registry = world.resource::<TickedComponentRegistry>().clone();
                registry.capture_all(world, tick);
            }
            ManualControlAction::StepBackward => {
                let current_tick = world.resource::<CurrentTick>().0;
                if current_tick == 0 {
                    continue;
                }
                let target = current_tick - 1;
                let registry = world.resource::<TickedComponentRegistry>().clone();
                registry.restore_all(world, target);
                world.resource_mut::<CurrentTick>().0 = target;
            }
            ManualControlAction::Reset(target) => {
                let current_tick = world.resource::<CurrentTick>().0;
                if target <= current_tick {
                    let registry = world.resource::<TickedComponentRegistry>().clone();
                    registry.restore_all(world, target);
                    world.resource_mut::<CurrentTick>().0 = target;
                } else {
                    rollback_and_resimulate(world, current_tick, target, TickedSimulation);
                }
            }
        }
    }
}
