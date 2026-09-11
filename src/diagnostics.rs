//! What a tick costs, measured around the tick and nowhere else.
//!
//! A frame time is a measurement of the compositor: an unfocused window is capped by the OS, a
//! vsync'd one by the display, and neither says anything about the simulation. One consumer
//! measured its frames for a week before noticing the number was about the window. The tick is
//! the thing this crate runs, so the tick is the thing this crate measures — around
//! [`run_tick_schedule`](crate::time::run_tick_schedule), which every path that advances a tick
//! goes through, including a rollback replaying a dozen of them in one frame.
//!
//! Replays count as ticks. That is the point: a client that replays its whole lead on every
//! snapshot spends seven ticks per real tick, and a number that hid that would be the same
//! number the frame time was.

use std::time::Duration;

use bevy::prelude::*;

/// Cumulative cost of every tick this app has run, replays included.
///
/// Present whenever [`TickedPlugin`](crate::TickedPlugin) is; updated by
/// [`run_tick_schedule`](crate::time::run_tick_schedule). Read it from a diagnostic overlay or a
/// test; reset it with [`reset`](Self::reset) to measure a window.
#[derive(Resource, Debug, Clone, Copy, Default)]
pub struct TickCost {
    /// Ticks run, replays included.
    pub ticks: u64,
    /// Wall-clock time spent inside ticks.
    pub spent: Duration,
    /// The longest single tick.
    pub worst: Duration,
}

impl TickCost {
    /// Mean time per tick, or zero if none has run.
    pub fn mean(&self) -> Duration {
        if self.ticks == 0 {
            Duration::ZERO
        } else {
            self.spent / self.ticks as u32
        }
    }

    pub fn reset(&mut self) {
        *self = Self::default();
    }

    pub(crate) fn record(&mut self, elapsed: Duration) {
        self.ticks += 1;
        self.spent += elapsed;
        self.worst = self.worst.max(elapsed);
    }
}
