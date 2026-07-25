//! Sub-tick interpolation.
//!
//! The simulation only advances on fixed ticks, but rendering runs every frame.
//! To avoid visible stutter, a visual that tracks a simulated value blends
//! between its previous- and current-tick states using the fraction of the way
//! through the pending tick (`Time<Fixed>::overstep_fraction`).
//!
//! [`TickInterpolation`] bundles [`CurrentTick`] with that always-clamped
//! fraction behind one consistent API, so interpolated visuals can't drift out
//! of sync with each other or re-derive the idiom by hand.

use bevy::ecs::system::SystemParam;
use bevy::prelude::*;

use crate::tick::CurrentTick;

/// Read-only access to the current sub-tick interpolation state.
///
/// Bundles the current simulation tick with the fractional progress through the
/// next tick. Use it in any `Update`/`PostUpdate` rendering system that needs to
/// smooth a tick-driven value across frames.
#[derive(SystemParam)]
pub struct TickInterpolation<'w> {
    current_tick: Res<'w, CurrentTick>,
    fixed_time: Res<'w, Time<Fixed>>,
}

impl TickInterpolation<'_> {
    /// Fraction in `[0, 1]` of the way from the last completed tick to the next.
    ///
    /// This is the blend factor for a two-point interpolation between a value's
    /// previous-tick and current-tick states.
    pub fn fraction(&self) -> f32 {
        self.fixed_time.overstep_fraction().clamp(0.0, 1.0)
    }

    /// The current simulation tick.
    pub fn current_tick(&self) -> u64 {
        self.current_tick.0
    }

    /// Continuous number of ticks elapsed since `tick`, including the current
    /// sub-tick [`fraction`](Self::fraction).
    ///
    /// Saturates at the fractional part for ticks at or in the future, so a
    /// freshly stamped event animates from zero rather than jumping.
    pub fn ticks_since(&self, tick: u64) -> f32 {
        self.current_tick.0.saturating_sub(tick) as f32 + self.fraction()
    }
}
