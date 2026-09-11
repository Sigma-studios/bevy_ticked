//! The simulation clock.
//!
//! The tick simulation needs its own clock, separate from `Time<Fixed>`. Two
//! reasons:
//!
//! 1. **Correctness.** Systems inside [`TickedSimulation`](crate::TickedSimulation)
//!    read the generic [`Time`] resource to integrate — most importantly avian,
//!    which takes its physics timestep straight from `Time::delta()`. Bevy only
//!    sets the generic `Time` to `Time<Fixed>` *while `FixedMain` is running*, so
//!    a tick advanced from anywhere else (manual stepping, rollback, a client
//!    replaying predicted ticks) would silently integrate by the render frame's
//!    delta instead of one tick.
//! 2. **Ownership.** A rollback simulation has to be able to move its clock
//!    backwards, and to run at a rate that is not Bevy's fixed timestep.
//!    `Time<Fixed>` is shared with everything else in `FixedUpdate` and can do
//!    neither.
//!
//! [`run_tick_schedule`] is the single entry point that installs the clock for
//! the duration of one tick. Every path that advances a tick goes through it.
//!
//! The clock is derived from the tick *counter*, never accumulated: elapsed time
//! at tick `n` is always exactly `n * timestep`. Accumulating would make a
//! replayed tick land on a slightly different elapsed value than the original,
//! which is precisely the kind of drift rollback cannot tolerate.

use core::time::Duration;

use bevy::ecs::schedule::ScheduleLabel;
use bevy::prelude::*;

use crate::tick::SECONDS_PER_TICK;

/// Context for [`Time<Ticked>`], the clock that measures simulation time.
///
/// One tick advances it by exactly [`timestep`](Time::timestep), regardless of
/// how much wall-clock time passed, so simulation time is a pure function of the
/// tick counter and is identical on every peer.
#[derive(Debug, Clone, Copy)]
pub struct Ticked {
    timestep: Duration,
    overstep: Duration,
}

impl Default for Ticked {
    fn default() -> Self {
        Self {
            timestep: Duration::from_secs_f32(SECONDS_PER_TICK),
            overstep: Duration::ZERO,
        }
    }
}

/// Accessors for [`Time<Ticked>`].
///
/// An extension trait rather than an inherent `impl` because [`Time`] is
/// defined upstream — Bevy can write `impl Time<Fixed>` only because `Fixed`
/// lives in the same crate as `Time`.
pub trait TickedTime {
    /// How much simulation time one tick is worth.
    fn timestep(&self) -> Duration;

    /// Set how much simulation time one tick is worth.
    ///
    /// # Panics
    ///
    /// Panics if `timestep` is zero.
    fn set_timestep(&mut self, timestep: Duration);

    /// Set the timestep as a frequency in hertz (`timestep = 1 / hz`).
    ///
    /// # Panics
    ///
    /// Panics if `hz` is zero, negative or not finite.
    fn set_timestep_hz(&mut self, hz: f64);

    /// Simulation time elapsed at the end of `tick`, i.e. `tick * timestep`.
    fn elapsed_at_tick(&self, tick: u64) -> Duration;

    /// Time accumulated toward the next tick but not yet consumed by one.
    fn overstep(&self) -> Duration;

    /// [`overstep`](Self::overstep) as a fraction of one timestep, clamped to
    /// `[0, 1]`.
    ///
    /// This is the blend factor for interpolating a visual between its
    /// previous-tick and current-tick states.
    fn overstep_fraction(&self) -> f32;

    /// Add to the accumulator. Normally only the tick driver calls this.
    fn accumulate(&mut self, delta: Duration);

    /// Set the accumulator directly.
    ///
    /// Used to mirror `Time<Fixed>` under [`TickSource::FixedUpdate`], and
    /// available to consumers driving their own clock who want to control the
    /// interpolation blend factor (e.g. scrubbing to a sub-tick position).
    ///
    /// [`TickSource::FixedUpdate`]: crate::TickSource::FixedUpdate
    fn set_overstep(&mut self, overstep: Duration);

    /// Take one timestep out of the accumulator if there is one, reporting
    /// whether a tick is owed.
    fn expend(&mut self) -> bool;

    /// Throw away accumulated time without running ticks for it.
    fn discard_overstep(&mut self);
}

impl TickedTime for Time<Ticked> {
    #[inline]
    fn timestep(&self) -> Duration {
        self.context().timestep
    }

    #[inline]
    fn set_timestep(&mut self, timestep: Duration) {
        assert!(
            !timestep.is_zero(),
            "attempted to set the tick timestep to zero"
        );
        self.context_mut().timestep = timestep;
    }

    #[inline]
    fn set_timestep_hz(&mut self, hz: f64) {
        assert!(hz.is_sign_positive() && hz != 0.0, "hz must be positive");
        assert!(hz.is_finite(), "hz must be finite");
        self.set_timestep(Duration::from_secs_f64(1.0 / hz));
    }

    #[inline]
    fn elapsed_at_tick(&self, tick: u64) -> Duration {
        elapsed_at(self.timestep(), tick)
    }

    #[inline]
    fn overstep(&self) -> Duration {
        self.context().overstep
    }

    #[inline]
    fn overstep_fraction(&self) -> f32 {
        (self.overstep().as_secs_f32() / self.timestep().as_secs_f32()).clamp(0.0, 1.0)
    }

    #[inline]
    fn accumulate(&mut self, delta: Duration) {
        self.context_mut().overstep += delta;
    }

    #[inline]
    fn set_overstep(&mut self, overstep: Duration) {
        self.context_mut().overstep = overstep;
    }

    #[inline]
    fn expend(&mut self) -> bool {
        let timestep = self.timestep();
        match self.context().overstep.checked_sub(timestep) {
            Some(remaining) => {
                self.context_mut().overstep = remaining;
                true
            }
            None => false,
        }
    }

    #[inline]
    fn discard_overstep(&mut self) {
        self.context_mut().overstep = Duration::ZERO;
    }
}

/// Multiplier applied to the time fed into the tick accumulator.
///
/// Present only under [`TickSource::Hz`](crate::TickSource::Hz), which is the
/// only source that owns an accumulator to stretch. `1.0` runs at the nominal
/// rate; `1.02` runs 2% fast, `0.98` 2% slow.
///
/// This is how a networked client steers its prediction lead. Nudging the rate
/// by a couple of percent moves the client relative to the server continuously
/// and invisibly, where adding or dropping a whole tick to correct the same
/// error is a visible discontinuity in everything the simulation drives. Keep
/// the deviation small: this is a control input to a feedback loop, and large
/// gains make it hunt.
#[derive(Resource, Debug, Clone, Copy, PartialEq)]
pub struct TickRateDilation(pub f64);

impl Default for TickRateDilation {
    fn default() -> Self {
        Self(1.0)
    }
}

/// `tick * timestep`, computed in integer nanoseconds so it is exact and
/// reproducible rather than drifting with repeated float accumulation.
fn elapsed_at(timestep: Duration, tick: u64) -> Duration {
    Duration::from_nanos((timestep.as_nanos() as u64).saturating_mul(tick))
}

/// Run `schedule` as simulation tick `tick`, with [`Time<Ticked>`] installed as
/// the generic [`Time`] resource for its duration.
///
/// This is what makes a tick mean the same thing everywhere. Inside the
/// schedule, `Time::delta()` is one tick's worth of simulation time and
/// `Time::elapsed()` is `tick * timestep` — whether this call came from the
/// `FixedUpdate` driver, a manual step, or a rollback replaying a hundred ticks
/// in one frame. The previous generic `Time` is restored afterwards, so systems
/// outside the simulation are unaffected.
///
/// The clock is rebuilt from `tick` on every call rather than advanced, so
/// re-running a tick during rollback reproduces the original values exactly and
/// moving backwards is not a special case.
///
/// The frame clocks are swapped too. `Time<Virtual>`, `Time<Fixed>` and `Time<Real>` read the
/// tick's delta and elapsed for the duration of the schedule, and are put back afterwards. A
/// system that reaches for `Time<Virtual>` inside the simulation — a timer, a cooldown, a
/// tween — used to get the *frame's* delta, which is one value on the tick that first ran and
/// another on the replay, and the replay diverged by the difference. Now it gets the tick's,
/// same as `Time`. The source guard in `bevy_ticked_testing` still flags the read, because
/// `Time<Real>` inside a tick is a wrong question even when the answer is made harmless.
pub fn run_tick_schedule(world: &mut World, tick: u64, schedule: impl ScheduleLabel) {
    let context = *world.resource::<Time<Ticked>>().context();

    // Build the clock so that `elapsed == tick * timestep` and `delta == timestep`.
    let mut clock = Time::new_with(context);
    clock.advance_by(elapsed_at(context.timestep, tick.saturating_sub(1)));
    clock.advance_by(context.timestep);
    *world.resource_mut::<Time<Ticked>>() = clock;

    let outer = *world.resource::<Time>();
    *world.resource_mut::<Time>() = clock.as_generic();
    let frame_clocks = swap_frame_clocks(world, &clock);

    let started = std::time::Instant::now();
    world.run_schedule(schedule);
    let elapsed = started.elapsed();

    restore_frame_clocks(world, frame_clocks);
    *world.resource_mut::<Time>() = outer;
    if let Some(mut cost) = world.get_resource_mut::<crate::diagnostics::TickCost>() {
        cost.record(elapsed);
    }
}

/// The frame clocks as they were outside the tick, so they can be put back.
struct FrameClocks {
    virtual_: Option<Time<Virtual>>,
    fixed: Option<Time<Fixed>>,
    real: Option<Time<Real>>,
}

/// Point every frame clock at the tick's delta and elapsed. Each keeps its own context (pause
/// state, relative speed, timestep), only the readings change.
fn swap_frame_clocks(world: &mut World, tick: &Time<Ticked>) -> FrameClocks {
    let delta = tick.delta();
    let elapsed = tick.elapsed();
    fn reclocked<T: Default + Clone>(
        outer: &Time<T>,
        elapsed: Duration,
        delta: Duration,
    ) -> Time<T> {
        let mut clock = Time::new_with(outer.context().clone());
        clock.advance_by(elapsed.saturating_sub(delta));
        clock.advance_by(delta);
        clock
    }
    let virtual_ = world.get_resource::<Time<Virtual>>().cloned();
    let fixed = world.get_resource::<Time<Fixed>>().cloned();
    let real = world.get_resource::<Time<Real>>().cloned();
    if let Some(outer) = &virtual_ {
        *world.resource_mut::<Time<Virtual>>() = reclocked(outer, elapsed, delta);
    }
    if let Some(outer) = &fixed {
        *world.resource_mut::<Time<Fixed>>() = reclocked(outer, elapsed, delta);
    }
    if let Some(outer) = &real {
        *world.resource_mut::<Time<Real>>() = reclocked(outer, elapsed, delta);
    }
    FrameClocks {
        virtual_,
        fixed,
        real,
    }
}

fn restore_frame_clocks(world: &mut World, clocks: FrameClocks) {
    if let Some(outer) = clocks.virtual_ {
        *world.resource_mut::<Time<Virtual>>() = outer;
    }
    if let Some(outer) = clocks.fixed {
        *world.resource_mut::<Time<Fixed>>() = outer;
    }
    if let Some(outer) = clocks.real {
        *world.resource_mut::<Time<Real>>() = outer;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clock() -> Time<Ticked> {
        Time::<Ticked>::default()
    }

    #[test]
    fn default_timestep_matches_the_tick_rate_constant() {
        assert_eq!(
            clock().timestep(),
            Duration::from_secs_f32(SECONDS_PER_TICK),
            "the default clock must tick at TICKS_PER_SECOND"
        );
        assert_eq!(clock().timestep(), Duration::from_micros(15_625));
    }

    #[test]
    fn elapsed_is_exact_and_linear_in_the_tick_count() {
        let c = clock();
        assert_eq!(c.elapsed_at_tick(0), Duration::ZERO);
        assert_eq!(c.elapsed_at_tick(64), Duration::from_secs(1));
        assert_eq!(c.elapsed_at_tick(64 * 3600), Duration::from_secs(3600));
    }

    #[test]
    fn elapsed_does_not_drift_over_a_long_run() {
        // The whole point of deriving from the counter: after an hour of ticks
        // the clock is still exactly on the second, which repeated float
        // accumulation would not be.
        let c = clock();
        let one_hour = 64 * 3600;
        assert_eq!(c.elapsed_at_tick(one_hour), Duration::from_secs(3600));
        assert_eq!(
            c.elapsed_at_tick(one_hour) - c.elapsed_at_tick(one_hour - 1),
            c.timestep()
        );
    }

    #[test]
    fn set_timestep_hz_round_trips() {
        let mut c = clock();
        c.set_timestep_hz(30.0);
        assert_eq!(c.timestep(), Duration::from_secs_f64(1.0 / 30.0));
        assert_eq!(c.elapsed_at_tick(30), Duration::from_nanos(999_999_990));
    }

    #[test]
    #[should_panic(expected = "zero")]
    fn zero_timestep_is_rejected() {
        clock().set_timestep(Duration::ZERO);
    }
}
