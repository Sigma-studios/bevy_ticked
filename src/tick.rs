use bevy::prelude::*;
use serde::{Deserialize, Serialize};

/// The number of ticks per second (matches Bevy's default `FixedUpdate` rate).
pub const TICKS_PER_SECOND: f32 = 64.0;

/// The duration of a single tick in seconds (inverse of [`TICKS_PER_SECOND`]).
pub const SECONDS_PER_TICK: f32 = 1.0 / TICKS_PER_SECOND;

/// The current simulation tick. Advances by 1 each time the tick system steps.
#[derive(Resource, Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CurrentTick(pub u64);

/// Controls whether ticks advance automatically via FixedUpdate.
#[derive(Resource, Clone, Debug)]
pub struct TickConfig {
    pub paused: bool,
}

impl Default for TickConfig {
    fn default() -> Self {
        Self { paused: false }
    }
}

/// Message: advance one tick forward (used for manual stepping while paused).
#[derive(Message)]
pub struct StepForward;

/// Message: step one tick backward by restoring state from history.
#[derive(Message)]
pub struct StepBackward;

/// Message: reset to a specific tick by restoring state from history.
#[derive(Message)]
pub struct ResetToTick(pub u64);
