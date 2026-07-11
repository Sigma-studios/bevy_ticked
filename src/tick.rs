use bevy::prelude::*;
use serde::{Deserialize, Serialize};

/// The default number of ticks per second when the built-in `FixedUpdate`
/// driver is used (see [`TickedPlugin::auto_advance`](crate::TickedPlugin)).
///
/// This is the *integration timestep* for the simulation, not a wall-clock
/// claim: it only equals real time when ticks are auto-advanced from
/// `FixedUpdate` at the matching rate. Consumers that drive ticks manually
/// (playback / time-warp / rollback) are free to treat one tick as any fixed
/// amount of in-game time — the only requirement is that the value is the same
/// on every peer that shares history (e.g. lockstep networking).
pub const TICKS_PER_SECOND: f32 = 64.0;

/// The duration of a single tick in seconds (inverse of [`TICKS_PER_SECOND`]).
pub const SECONDS_PER_TICK: f32 = 1.0 / TICKS_PER_SECOND;

/// Default number of ticks of history to retain in `WorldActions`.
///
/// This is only the default for [`HistoryBufferTicks`]; the live value is a
/// resource and can be changed at runtime to trade memory for scrub depth.
pub const HISTORY_BUFFER_TICKS: u64 = 6400;

/// How many ticks of history to keep in `WorldActions` before pruning.
///
/// Defaults to [`HISTORY_BUFFER_TICKS`]. Raise it for a deeper scrub/rewind
/// window (at the cost of memory), lower it to bound memory more tightly.
#[derive(Resource, Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct HistoryBufferTicks(pub u64);

impl Default for HistoryBufferTicks {
    fn default() -> Self {
        Self(HISTORY_BUFFER_TICKS)
    }
}

/// The current simulation tick. Advances by 1 each time the tick system steps.
#[derive(Resource, Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CurrentTick(pub u64);

/// Marker resource: when present, tick advancement is paused.
///
/// Insert this resource to pause the simulation, remove it to resume.
#[derive(Resource, Clone, Copy, Debug, Default)]
pub struct TicksPaused;

/// Message: advance one tick forward (used for manual stepping while paused).
#[derive(Message)]
pub struct StepForward;

/// Message: step one tick backward by restoring state from history.
#[derive(Message)]
pub struct StepBackward;

/// Message: reset to a specific tick by restoring state from history.
#[derive(Message)]
pub struct ResetToTick(pub u64);
