use bevy::prelude::*;
use serde::{Deserialize, Serialize};

/// The number of ticks per second. This MUST match Bevy's `FixedUpdate` rate
/// (64 Hz by default). If you change one, change the other.
pub const TICKS_PER_SECOND: f32 = 64.0;

/// The duration of a single tick in seconds (inverse of [`TICKS_PER_SECOND`]).
pub const SECONDS_PER_TICK: f32 = 1.0 / TICKS_PER_SECOND;

/// Maximum number of ticks of history to retain in `WorldActions`.
pub const HISTORY_BUFFER_TICKS: u64 = 6400;

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
