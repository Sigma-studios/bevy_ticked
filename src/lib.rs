pub mod checksum;
pub mod diagnostics;
pub mod events;
pub mod interpolation;
pub mod prelude;
pub mod registry;
pub mod resource_registry;
pub mod rollback;
pub mod tick;
pub mod time;
pub mod tracked_entity;
pub mod tracked_index;
pub mod world_actions;

use bevy::app::{MainScheduleOrder, RunFixedMainLoop, RunFixedMainLoopSystems};
use bevy::ecs::schedule::ScheduleLabel;
use bevy::prelude::*;

use registry::TickedComponentRegistry;
use rollback::rollback_and_resimulate;
use tick::{
    CurrentTick, HistoryBufferTicks, ResetToTick, StepBackward, StepForward, TicksPaused,
};
use time::{run_tick_schedule, TickRateDilation, Ticked, TickedTime};
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

/// The schedule that hosts one tick's lifecycle.
///
/// This is the stable attachment point for anything that needs to run around
/// tick advancement: it always contains [`TickedSystems`] in order, no matter
/// what is driving the clock. Register here rather than in `FixedUpdate`, and
/// your systems keep working if the tick rate is later decoupled from Bevy's
/// fixed timestep.
///
/// Distinct from [`TickedSimulation`], which holds the game simulation itself
/// and is run once per tick from inside this schedule.
#[derive(ScheduleLabel, Clone, Debug, PartialEq, Eq, Hash)]
pub struct TickedLoop;

/// System sets for ordering relative to tick advancement, within [`TickedLoop`].
#[derive(SystemSet, Clone, Debug, PartialEq, Eq, Hash)]
pub enum TickedSystems {
    /// The first thing in the loop: put the world back the way the simulation left it.
    ///
    /// Anything that writes a *presentation* value into a simulated component between ticks —
    /// [`TickedInterpolationPlugin`](interpolation::TickedInterpolationPlugin) blending a
    /// `Transform` for the renderer — undoes that write here, before any system in the loop
    /// reads the component. Nothing else belongs in this set.
    Restore,
    /// Runs before tick advancement (e.g. client rollback on snapshot).
    PreTick,
    /// The core tick advancement: increment, run TickedSimulation, capture.
    Tick,
    /// Runs after tick advancement (e.g. server snapshot broadcast, client input send).
    PostTick,
}

/// The schedule that drives [`TickedLoop`] when the tick rate is decoupled from
/// Bevy's fixed timestep.
///
/// Inserted immediately after `RunFixedMainLoop`, so ticks occupy the same slot
/// in the frame that `FixedMain` does and input latency is unchanged. Only
/// present for [`TickSource::Hz`] and [`TickSource::Manual`].
#[derive(ScheduleLabel, Clone, Debug, PartialEq, Eq, Hash)]
pub struct RunTickedLoop;

/// What advances the tick clock.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TickSource {
    /// One tick per Bevy `FixedUpdate` step, at Bevy's fixed timestep.
    ///
    /// The default, and the right choice unless you need otherwise: every tick
    /// stays bracketed by `FixedMain`, so systems in `FixedPreUpdate` /
    /// `FixedPostUpdate` interleave with ticks the way they always have.
    FixedUpdate,
    /// Own accumulator, running `hz` ticks per second of virtual time.
    ///
    /// Fed from `Time<Virtual>`, so pausing, `set_relative_speed` (slow motion,
    /// fast forward) and the spiral-of-death guard of `Time<Virtual>::max_delta`
    /// all apply for free. This is the mode that decouples the simulation rate
    /// from Bevy's fixed timestep and the only one where the accumulator can be
    /// dilated to steer a networked client's prediction lead.
    Hz(f64),
    /// Nothing advances the clock automatically.
    ///
    /// Ticks move only via [`StepForward`] / [`StepBackward`] / [`ResetToTick`],
    /// or by running [`TickedLoop`] yourself. For playback, scrubbing and
    /// frame-by-frame debugging.
    Manual,
}

impl TickSource {
    /// Whether this source can be steered: run a little fast or slow to move a networked
    /// client's prediction lead without adding or dropping whole ticks.
    ///
    /// Only [`Hz`](Self::Hz) owns an accumulator to stretch. [`Manual`](Self::Manual) is
    /// steered by whoever drives it, which a test harness does. [`FixedUpdate`](Self::FixedUpdate)
    /// hands the clock to Bevy and can only nudge by a whole tick, so a networked client on it
    /// jumps a tick every time its lead drifts — visibly, in everything the simulation draws.
    pub fn is_steerable(&self) -> bool {
        !matches!(self, Self::FixedUpdate)
    }
}

/// Which [`TickSource`] the app was built with, for anything that has to know.
///
/// Inserted by [`TickedPlugin`]. The networking plugins read it to refuse a source they cannot
/// steer (see [`require_steerable_tick_source`]).
#[derive(Resource, Debug, Clone, Copy, PartialEq)]
pub struct ConfiguredTickSource(pub TickSource);

/// Refuse to build a networked role on a tick source that cannot be steered.
///
/// Call it from a plugin's `finish`, where every plugin's `build` has run. Panics with a
/// message naming `plugin` and the fix, because the alternative was what every consumer had:
/// a client that ran on `FixedUpdate` for months, nudging its lead by a whole tick at a time,
/// and a bug report that said "the remote players stutter".
///
/// # Panics
///
/// If [`TickedPlugin`] was not added, or was added with [`TickSource::FixedUpdate`].
pub fn require_steerable_tick_source(app: &App, plugin: &str) {
    let Some(source) = app.world().get_resource::<ConfiguredTickSource>() else {
        panic!("{plugin} needs TickedPlugin, added before it");
    };
    assert!(
        source.0.is_steerable(),
        "{plugin} cannot run on TickSource::FixedUpdate: a networked client steers its \
         prediction lead by running a couple of percent fast or slow, and Bevy's fixed clock \
         cannot be stretched. Build TickedPlugin with `source: TickSource::Hz(64.0)` (the \
         simulation rate you had) and add TickedInterpolationPlugin for smooth visuals. \
         TickedPlugin::default() keeps FixedUpdate for solo play and scrubbing."
    );
}

/// Plugin that installs the deterministic tick simulation.
///
/// Defaults to one tick per `FixedUpdate` step, which is the classic
/// fixed-timestep behavior. Change [`source`](Self::source) to decouple the
/// simulation rate from Bevy's fixed timestep.
pub struct TickedPlugin {
    /// What advances the tick clock. Defaults to [`TickSource::FixedUpdate`].
    pub source: TickSource,
    /// Ceiling on how many ticks one frame may run to catch up, for
    /// [`TickSource::Hz`].
    ///
    /// `Time<Virtual>::max_delta` already bounds the accumulated time, but a
    /// tick here can drag a rollback resimulation behind it and so is far more
    /// expensive than a plain fixed step. When the ceiling is hit the remaining
    /// accumulated time is dropped rather than queued, trading a small time
    /// discontinuity for not spiralling.
    pub max_ticks_per_frame: u32,
    /// How many ticks of history to keep, or `None` to let whoever knows better decide.
    ///
    /// `None` installs [`HISTORY_BUFFER_TICKS`](tick::HISTORY_BUFFER_TICKS) (a hundred
    /// seconds, for scrubbing) and marks it as a default, which a plugin added later may
    /// replace — the networking plugins size it from what a rollback can actually reach.
    /// `Some(n)` is a game's own choice and nothing replaces it. See
    /// [`HistoryWindowChosen`].
    pub history_ticks: Option<u64>,
    /// How [`TickedSimulation`] runs its systems.
    ///
    /// Single-threaded by default, because the simulation is supposed to be deterministic and
    /// a multi-threaded executor is the easiest way to lose that: two systems that both write a
    /// component with no ordering between them run in an order that differs per frame and per
    /// peer, and the world hash disagrees for a reason no test can reproduce. A game whose
    /// systems are all explicitly ordered can take `MultiThreaded` back; it gains little, since
    /// a tick is short and the executor's overhead is not.
    pub simulation_executor: SimulationExecutor,
}

/// Which executor runs [`TickedSimulation`]. See [`TickedPlugin::simulation_executor`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SimulationExecutor {
    /// Systems run one at a time, in a fixed order. Deterministic by construction.
    #[default]
    SingleThreaded,
    /// Bevy's default: systems without an ordering between them may run in any order.
    MultiThreaded,
}

impl Default for TickedPlugin {
    fn default() -> Self {
        Self {
            source: TickSource::FixedUpdate,
            // 250ms of catch-up at 64Hz, matching Time<Virtual>'s default
            // max_delta.
            max_ticks_per_frame: 16,
            history_ticks: None,
            simulation_executor: SimulationExecutor::SingleThreaded,
        }
    }
}

/// Runtime copy of [`TickedPlugin::max_ticks_per_frame`].
#[derive(Resource, Clone, Copy, Debug)]
pub struct MaxTicksPerFrame(pub u32);

/// Present when the game chose its own [`HistoryBufferTicks`] through
/// [`TickedPlugin::history_ticks`]. A plugin with a better default for its role replaces the
/// window only when this is absent.
#[derive(Resource, Clone, Copy, Debug)]
pub struct HistoryWindowChosen;

impl Plugin for TickedPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<CurrentTick>()
            .init_resource::<TickedComponentRegistry>()
            .init_resource::<resource_registry::TickedResourceRegistry>()
            .init_resource::<TickTrackedEntityCounter>()
            .init_resource::<tracked_index::TrackedEntityIndex>()
            .add_observer(tracked_index::index_tracked)
            .add_observer(tracked_index::unindex_tracked)
            .insert_resource(ConfiguredTickSource(self.source))
            .init_resource::<Time<Ticked>>()
            .init_resource::<diagnostics::TickCost>()
            .init_schedule(TickedSimulation)
            .init_schedule(TickedLoop)
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

        // The tick lifecycle lives in `TickedLoop` unconditionally, so that
        // downstream ordering against `TickedSystems` holds whatever drives the
        // clock. Previously these sets were only configured when auto-advancing,
        // which silently left the whole networking stack unordered in manual
        // mode -- `.after(set)` on a set with no members is a no-op, not an error.
        app.configure_sets(
            TickedLoop,
            (
                TickedSystems::Restore,
                TickedSystems::PreTick,
                TickedSystems::Tick,
                TickedSystems::PostTick,
            )
                .chain(),
        )
        .add_systems(TickedLoop, advance_tick_system.in_set(TickedSystems::Tick));
        if self.simulation_executor == SimulationExecutor::SingleThreaded {
            app.edit_schedule(TickedSimulation, |schedule| {
                schedule.set_executor(bevy::ecs::schedule::SingleThreadedExecutor::new());
            });
        }

        app.insert_resource(MaxTicksPerFrame(self.max_ticks_per_frame));
        match self.history_ticks {
            Some(ticks) => {
                app.insert_resource(HistoryBufferTicks(ticks))
                    .insert_resource(HistoryWindowChosen);
            }
            None => {
                app.init_resource::<HistoryBufferTicks>();
            }
        }

        match self.source {
            TickSource::FixedUpdate => {
                app.add_systems(FixedUpdate, drive_ticked_loop_from_fixed_update)
                    .add_systems(
                        RunFixedMainLoop,
                        mirror_fixed_clock.in_set(RunFixedMainLoopSystems::AfterFixedMainLoop),
                    );
            }
            TickSource::Hz(hz) => {
                app.world_mut()
                    .resource_mut::<Time<Ticked>>()
                    .set_timestep_hz(hz);
                install_run_ticked_loop(app);
                app.init_resource::<TickRateDilation>()
                    .add_systems(RunTickedLoop, drive_ticked_loop_from_accumulator);
            }
            TickSource::Manual => {
                // The schedule exists so consumers can drive TickedLoop from it
                // on their own terms, but nothing is registered to advance time.
                install_run_ticked_loop(app);
            }
        }
    }
}

/// Present while a manual [`StepForward`] runs [`TickedLoop`], so the tick advances even though
/// the simulation is paused. Removed before the loop returns.
#[derive(Resource, Debug, Clone, Copy)]
pub struct StepOnce;

/// Present while [`TickedLoop`] runs after a manual [`StepBackward`] or [`ResetToTick`]: the
/// world has just been restored to [`CurrentTick`] and the tick must not advance, but everything
/// around it — the interpolation pair, a broadcast, a checksum — has to see the restored world.
/// Removed before the loop returns.
#[derive(Resource, Debug, Clone, Copy)]
pub struct RestoredThisPass;

/// Give [`RunTickedLoop`] the frame slot immediately after `FixedMain`.
fn install_run_ticked_loop(app: &mut App) {
    app.init_schedule(RunTickedLoop);
    app.world_mut()
        .resource_mut::<MainScheduleOrder>()
        .insert_after(RunFixedMainLoop, RunTickedLoop);
}

/// Run one pass of [`TickedLoop`] per `FixedUpdate` step.
///
/// Keeping the driver inside `FixedUpdate` means every tick stays bracketed by
/// `FixedMain`, so consumer systems in `FixedPreUpdate`/`FixedPostUpdate` still
/// interleave with ticks exactly as before.
fn drive_ticked_loop_from_fixed_update(world: &mut World) {
    world.run_schedule(TickedLoop);
}

/// Under [`TickSource::FixedUpdate`] the fixed clock *is* the tick clock, so
/// copy its timestep and leftover accumulator onto [`Time<Ticked>`].
///
/// This is what lets [`TickInterpolation`](interpolation::TickInterpolation)
/// read one clock in every mode. It runs after the last fixed step of the frame,
/// so the overstep it publishes is the final one for this frame — exactly what a
/// render system interpolating toward the next tick needs.
///
/// Mirroring the timestep too removes an old footgun: the simulation used to
/// integrate by the `SECONDS_PER_TICK` constant while `Time<Fixed>` could be set
/// to some other rate, with nothing reporting the mismatch.
fn mirror_fixed_clock(fixed: Res<Time<Fixed>>, mut ticked: ResMut<Time<Ticked>>) {
    ticked.set_timestep(fixed.timestep());
    ticked.set_overstep(fixed.overstep());
}

/// Run [`TickedLoop`] as many times as the accumulator has whole ticks for.
///
/// The accumulator is fed from `Time<Virtual>`, so pause and relative speed are
/// inherited, and `Time<Virtual>::max_delta` already bounds a single frame's
/// contribution. [`MaxTicksPerFrame`] bounds it a second time, because a tick
/// here can pull a rollback resimulation along with it.
fn drive_ticked_loop_from_accumulator(world: &mut World) {
    let dilation = world.resource::<TickRateDilation>().0;
    let delta = world.resource::<Time<Virtual>>().delta().mul_f64(dilation);
    world.resource_mut::<Time<Ticked>>().accumulate(delta);

    let max = world.resource::<MaxTicksPerFrame>().0;
    let mut ran = 0;
    while world.resource_mut::<Time<Ticked>>().expend() {
        world.run_schedule(TickedLoop);
        ran += 1;
        if ran >= max {
            // Give up on the rest of this frame's backlog rather than fall
            // further behind every frame.
            world.resource_mut::<Time<Ticked>>().discard_overstep();
            break;
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

    run_tick_schedule(world, tick, TickedSimulation);
    let registry = world.resource::<TickedComponentRegistry>().clone();
    registry.capture_all(world, tick);

    // Prune old history to prevent unbounded memory growth.
    let buffer = world.resource::<HistoryBufferTicks>().0;
    let prune_tick = tick.saturating_sub(buffer);
    if prune_tick > 0 {
        registry.prune_all_before(world, prune_tick);
    }
}

/// Auto-advance one tick per pass of the loop, unless paused.
///
/// A manual step passes with [`StepOnce`] present and advances regardless; a manual rewind
/// passes with [`RestoredThisPass`] present and never advances.
fn advance_tick_system(world: &mut World) {
    if world.contains_resource::<RestoredThisPass>() {
        return;
    }
    if world.contains_resource::<TicksPaused>() && !world.contains_resource::<StepOnce>() {
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

    // Every manual action goes through `TickedLoop`, not around it. A step used to call
    // `advance_one_tick` directly, so nothing in `PreTick` or `PostTick` ran: the interpolation
    // pair was not shifted, a host stepping its paused world broadcast nothing, a checksum was
    // not sampled. Scrubbing was a different simulation from playing.
    for action in actions {
        match action {
            ManualControlAction::StepForward => {
                world.insert_resource(StepOnce);
                world.run_schedule(TickedLoop);
                world.remove_resource::<StepOnce>();
            }
            ManualControlAction::StepBackward => {
                let current_tick = world.resource::<CurrentTick>().0;
                if current_tick == 0 {
                    continue;
                }
                restore_to(world, current_tick - 1);
                run_loop_restored(world);
            }
            ManualControlAction::Reset(target) => {
                let current_tick = world.resource::<CurrentTick>().0;
                if target <= current_tick {
                    restore_to(world, target);
                } else {
                    rollback_and_resimulate(world, current_tick, target, TickedSimulation);
                }
                run_loop_restored(world);
            }
        }
    }
}

fn restore_to(world: &mut World, target: u64) {
    let registry = world.resource::<TickedComponentRegistry>().clone();
    registry.restore_all(world, target);
    events::TickedEventRegistry::truncate_all_after(world, target);
    world.resource_mut::<CurrentTick>().0 = target;
}

fn run_loop_restored(world: &mut World) {
    world.insert_resource(RestoredThisPass);
    world.run_schedule(TickedLoop);
    world.remove_resource::<RestoredThisPass>();
}
