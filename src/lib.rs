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
use tick::{
    CurrentTick, HistoryBufferTicks, ResetToTick, StepBackward, StepForward, TicksPaused,
};
use tracked_entity::{TickTrackedEntity, TickTrackedEntityCounter};

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

/// Plugin that installs the deterministic tick simulation.
///
/// By default it auto-advances one tick per `FixedUpdate` step (frame-rate
/// independent, catch-up on lag — the classic fixed-timestep behavior). Set
/// [`auto_advance`](Self::auto_advance) to `false` to take full control of the
/// clock yourself: the plugin then never touches `FixedUpdate`, and ticks only
/// move when you send [`StepForward`] / [`StepBackward`] / [`ResetToTick`]
/// messages. This is what lets a consumer build playback (variable speed,
/// slow-motion, reverse) and scrubbing on top, using its own accumulator and
/// its own choice of in-game-seconds-per-tick, without the built-in driver
/// fighting it.
pub struct TickedPlugin {
    /// When `true` (default), advance one tick per `FixedUpdate` step unless
    /// [`TicksPaused`] is present. When `false`, the tick clock is entirely
    /// driven by the manual step/reset messages.
    pub auto_advance: bool,
}

impl Default for TickedPlugin {
    fn default() -> Self {
        Self { auto_advance: true }
    }
}

impl Plugin for TickedPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<CurrentTick>()
            .init_resource::<TickedComponentRegistry>()
            .init_resource::<TickTrackedEntityCounter>()
            .init_resource::<HistoryBufferTicks>()
            .init_schedule(TickedSimulation)
            .add_message::<StepForward>()
            .add_message::<StepBackward>()
            .add_message::<ResetToTick>()
            // Runs in both auto and manual mode: guarantees tick-0 is captured
            // (so reset/step-back to the start works) and applies manual
            // step/reset messages.
            .add_systems(
                PreUpdate,
                (ensure_initial_capture, apply_manual_controls).chain(),
            );

        if self.auto_advance {
            app.configure_sets(
                FixedUpdate,
                (TickedSet::PreTick, TickedSet::Tick, TickedSet::PostTick).chain(),
            )
            .add_systems(FixedUpdate, advance_tick_system.in_set(TickedSet::Tick));
        }
    }
}

/// Capture the initial world state at tick 0 exactly once, as soon as any
/// tracked components exist. Runs in every mode so that `ResetToTick(0)` and
/// stepping back to the start always have a snapshot to restore.
fn ensure_initial_capture(world: &mut World, mut done: Local<bool>) {
    if *done {
        return;
    }
    let registry = world.resource::<TickedComponentRegistry>().clone();
    if registry.is_empty() {
        return;
    }
    // Wait until at least one tracked entity exists before snapshotting tick 0.
    // Capturing an empty world (e.g. before entities finish spawning from an
    // async asset load) would make `ResetToTick(0)` strip components off every
    // entity later. An empty tick-0 snapshot is useless anyway.
    let mut tracked = world.query::<&TickTrackedEntity>();
    if tracked.iter(world).next().is_none() {
        return;
    }
    let current_tick = world.resource::<CurrentTick>().0;
    if current_tick == 0 && !registry.has_tick_captured(world, 0) {
        registry.capture_all(world, 0);
    }
    *done = true;
}

/// The core "advance one tick" step, shared by the `FixedUpdate` driver and the
/// manual `StepForward` path: increment the tick, run the simulation schedule,
/// capture the new state, and prune history beyond the retained window.
fn advance_one_tick(world: &mut World) {
    let tick = {
        let mut current = world.resource_mut::<CurrentTick>();
        current.0 += 1;
        current.0
    };

    world.run_schedule(TickedSimulation);
    let registry = world.resource::<TickedComponentRegistry>().clone();
    registry.capture_all(world, tick);

    // Prune old history to prevent unbounded memory growth.
    let buffer = world.resource::<HistoryBufferTicks>().0;
    let prune_tick = tick.saturating_sub(buffer);
    if prune_tick > 0 {
        registry.prune_all_before(world, prune_tick);
    }
}

/// Auto-advance one tick per `FixedUpdate` step, unless paused.
fn advance_tick_system(world: &mut World) {
    if world.get_resource::<TicksPaused>().is_some() {
        return;
    }
    advance_one_tick(world);
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
                advance_one_tick(world);
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
