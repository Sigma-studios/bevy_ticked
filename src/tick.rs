use std::collections::BTreeSet;

use bevy::prelude::*;
use serde::{Deserialize, Serialize};

use crate::tick_types::{Tick, Ticks};

/// The default number of ticks per second when the built-in `FixedUpdate`
/// driver is used (see [`TickedPlugin::auto_advance`](crate::TickedPlugin)).
///
/// This is the *integration timestep* for the simulation, not a wall-clock
/// claim: it only equals real time when ticks are auto-advanced from
/// `FixedUpdate` at the matching rate. Consumers that drive ticks manually
/// (playback / time-warp / rollback) are free to treat one tick as any fixed
/// amount of in-game time — the only requirement is that the value is the same
/// on every peer that shares history (e.g. lockstep networking).
pub const DEFAULT_TICK_HZ: u32 = 64;

/// The length of one tick at [`DEFAULT_TICK_HZ`]: 15.625 ms, exactly.
///
/// A default, as the name says. The live length is `Time<Ticked>::timestep()`, which a game can
/// set to anything; code that turns time into ticks reads that, or it is wrong for every game that
/// does. Inside the tick, read `Res<Time>::delta()`.
pub const DEFAULT_TIMESTEP: std::time::Duration = std::time::Duration::from_micros(15_625);

/// The old name of [`DEFAULT_TICK_HZ`].
#[deprecated(
    note = "a default, not the live rate: use `DEFAULT_TICK_HZ`, or the timestep of \
                     `Time<Ticked>` for the rate a game actually runs at"
)]
pub const TICKS_PER_SECOND: f32 = DEFAULT_TICK_HZ as f32;

/// The old name of [`DEFAULT_TIMESTEP`], in `f32` seconds.
#[deprecated(
    note = "a default, not the live tick: use `DEFAULT_TIMESTEP`, or the timestep of \
                     `Time<Ticked>` for the tick a game actually runs at"
)]
pub const SECONDS_PER_TICK: f32 = 1.0 / DEFAULT_TICK_HZ as f32;

/// Default number of ticks of history to retain in `WorldActions`.
///
/// This is only the default for [`HistoryBufferTicks`]; the live value is a
/// resource and can be changed at runtime to trade memory for scrub depth.
pub const HISTORY_BUFFER_TICKS: Ticks = Ticks(6400);

/// How many ticks of history to keep in `WorldActions` before pruning.
///
/// Defaults to [`HISTORY_BUFFER_TICKS`]. Raise it for a deeper scrub/rewind
/// window (at the cost of memory), lower it to bound memory more tightly.
#[derive(Resource, Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct HistoryBufferTicks(pub Ticks);

impl Default for HistoryBufferTicks {
    fn default() -> Self {
        Self(HISTORY_BUFFER_TICKS)
    }
}

/// The current simulation tick. Advances by 1 each time the tick system steps.
#[derive(Resource, Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CurrentTick(pub Tick);

/// Why the clock is not advancing.
///
/// Several parts of a session have a reason to stop the clock, and they used to share one
/// marker resource: a joining client set it until its first snapshot, a lockstep peer set
/// and cleared it every frame from what it had received, a game set it for the pause menu,
/// and each `remove_resource` lifted everybody's. A user's pause was undone by the first
/// snapshot to arrive; a lockstep client's wait was undone by the game's unpause. Now every
/// subsystem holds its own reason and releases only that one; the clock advances when nobody
/// holds it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum TickHoldReason {
    /// The game asked: a pause menu, a debugger, a scrub bar.
    Manual,
    /// A client that has joined and not yet received the world it is joining.
    AwaitingSync,
    /// The session is paused for everybody, on the authority's word.
    SessionPause,
    /// A lockstep peer waiting for the actions or the authoritative tick it needs to advance.
    WaitingForPeers,
    /// A client that has not heard from its host for long enough to stop predicting into a
    /// future the host may never produce.
    SoftHold,
    /// A client in the middle of a replay it could not finish in one frame: the clock waits
    /// for the rest of it rather than running ahead of a world that is not caught up.
    Replaying,
    /// The session's host is gone and a new one is taking over: a lockstep peer waits until the
    /// survivors have agreed on the last tick the old host ruled.
    HostMigration,
    /// A game's own reason, for something not listed.
    Custom(u8),
}

/// Every reason the clock is currently held, and the rule that it advances only when there
/// is none. See [`TickHoldReason`].
///
/// Hold and release your own reason and nobody else's: `hold(Manual)` for a pause menu,
/// `release(Manual)` when it closes. Whether the clock is stopped is [`is_held`](Self::is_held);
/// why is [`reasons`](Self::reasons).
#[derive(Resource, Clone, Debug, Default, PartialEq, Eq)]
pub struct TickHolds(BTreeSet<TickHoldReason>);

impl TickHolds {
    /// Stop the clock for `reason`. Holding a reason already held changes nothing.
    pub fn hold(&mut self, reason: TickHoldReason) {
        self.0.insert(reason);
    }

    /// Let go of `reason`. Returns whether it was held. The clock runs again only if this was
    /// the last reason.
    pub fn release(&mut self, reason: TickHoldReason) -> bool {
        self.0.remove(&reason)
    }

    /// Hold or release `reason` from a boolean, for a subsystem that decides every frame.
    pub fn set(&mut self, reason: TickHoldReason, held: bool) {
        if held {
            self.hold(reason);
        } else {
            self.release(reason);
        }
    }

    /// Whether anything is holding the clock.
    pub fn is_held(&self) -> bool {
        !self.0.is_empty()
    }

    /// Whether `reason` in particular is holding it.
    pub fn holds(&self, reason: TickHoldReason) -> bool {
        self.0.contains(&reason)
    }

    /// Every reason currently held, in a fixed order.
    pub fn reasons(&self) -> impl Iterator<Item = TickHoldReason> + '_ {
        self.0.iter().copied()
    }

    /// Whether the only thing holding the clock is `reason`.
    pub fn held_only_by(&self, reason: TickHoldReason) -> bool {
        self.0.len() == 1 && self.holds(reason)
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
pub struct ResetToTick(pub Tick);
