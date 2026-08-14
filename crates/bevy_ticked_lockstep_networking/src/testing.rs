//! Asking a peer what its lockstep state is, for tests.
//!
//! A netcode test spends most of its length asking the same handful of questions — is the
//! simulation keeping up, what did the buffer settle on, is the scheduled-tick sequence
//! contiguous, how far behind is this peer — and every consumer was writing its own accessors
//! for them, reaching into this crate's resources by hand.
//!
//! These are those questions. They take an [`App`], so they compose with whatever drives the
//! peers: `bevy_ensemble_loopback`'s `LoopbackNetwork`, a multi-process harness, or a game's own
//! wrapper around either. That is deliberate — the *driver* is a transport concern and already
//! lives in the loopback crate, and what was missing was never the loop but the vocabulary.
//!
//! # Why these particular ones
//!
//! Each of them was written to catch something that actually happened:
//!
//! * [`tracker_gap`] and [`last_scheduled_tick`] exist because a buffer that grew between two
//!   flushes left a hole in the scheduled-tick sequence, and the host waited on the missing tick
//!   for ever. A hole here is a hung session that has not hung yet.
//! * [`client_tick_buffer`] and [`host_tick_buffer`] exist because "the buffer adapts" is not a
//!   claim a test can make without reading what it adapted *to*.
//! * [`tracked_ticks`] and [`actions_at`] are how you tell "the action was lost" from "the action
//!   arrived and did nothing", which look identical from the far side of a simulation.

use bevy::prelude::*;
use bevy_ticked::prelude::{CurrentTick, TicksPaused};

use crate::{
    ActionTracker, LastScheduledTick, LocalPendingActions, LockstepAction, LockstepConfig,
};

/// The tick this peer has simulated up to.
pub fn current_tick(app: &App) -> u64 {
    app.world().resource::<CurrentTick>().0
}

/// Whether this peer's simulation is currently held.
///
/// On a client that usually means it is waiting for an authoritative tick, which is the normal
/// resting state of a peer that has caught up — not a fault by itself.
pub fn is_paused(app: &App) -> bool {
    app.world().get_resource::<TicksPaused>().is_some()
}

/// What the client-side buffer has settled on, in ticks.
pub fn client_tick_buffer(app: &App) -> u64 {
    app.world().resource::<LockstepConfig>().client_tick_buffer
}

/// What the host-side buffer has settled on, in ticks.
pub fn host_tick_buffer(app: &App) -> u64 {
    app.world().resource::<LockstepConfig>().host_tick_buffer
}

/// The newest tick this peer has scheduled actions for.
///
/// Sits `buffer` ahead of its current tick. What matters is that the sequence leading up to it
/// has no holes — see [`tracker_gap`].
pub fn last_scheduled_tick(app: &App) -> u64 {
    app.world().resource::<LastScheduledTick>().0.unwrap_or(0)
}

/// Which ticks this peer has any action data for, ascending.
pub fn tracked_ticks<A: LockstepAction>(app: &App) -> Vec<u64> {
    let mut ticks: Vec<u64> = app
        .world()
        .resource::<ActionTracker<A>>()
        .ticks
        .keys()
        .copied()
        .collect();
    ticks.sort_unstable();
    ticks
}

/// The first tick missing from an otherwise contiguous run in this peer's tracker.
///
/// A hole here is a hung session waiting to happen: the host requires an entry from every
/// established participant for every tick, and there is no gap-fill and no timeout.
///
/// Old ticks are pruned from the front as they are simulated, so only a gap *inside* the retained
/// window counts — this reports `None` for a window that simply starts late.
pub fn tracker_gap<A: LockstepAction>(app: &App) -> Option<u64> {
    tracked_ticks::<A>(app)
        .windows(2)
        .find(|pair| pair[1] != pair[0] + 1)
        .map(|pair| pair[0] + 1)
}

/// Every action recorded for `tick`, flattened across players in the tracker's own order.
pub fn actions_at<A: LockstepAction>(app: &App, tick: u64) -> Vec<A> {
    app.world()
        .resource::<ActionTracker<A>>()
        .actions_for_tick(tick)
        .map(|players| players.values().flatten().cloned().collect())
        .unwrap_or_default()
}

/// Queue an action as though this peer's local input had produced it.
///
/// It is scheduled by the next flush, `buffer` ticks ahead, exactly as a real one would be — so a
/// test that pushes an action and then asserts about the very next tick is asserting about the
/// wrong tick.
pub fn push_action<A: LockstepAction>(app: &mut App, action: A) {
    if let Some(mut pending) = app.world_mut().get_resource_mut::<LocalPendingActions<A>>() {
        pending.0.push(action);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app_with(ticks: &[u64]) -> App {
        let mut app = App::new();
        let mut tracker = ActionTracker::<u8>::default();
        for tick in ticks {
            tracker.ticks.entry(*tick).or_default();
        }
        app.insert_resource(tracker);
        app
    }

    #[test]
    fn a_contiguous_sequence_has_no_gap() {
        let app = app_with(&[4, 5, 6, 7]);
        assert_eq!(tracker_gap::<u8>(&app), None);
        assert_eq!(tracked_ticks::<u8>(&app), vec![4, 5, 6, 7]);
    }

    #[test]
    fn the_gap_reported_is_the_first_missing_tick() {
        let app = app_with(&[4, 5, 9, 10]);
        assert_eq!(
            tracker_gap::<u8>(&app),
            Some(6),
            "the hole starts at 6; reporting 9 would name the tick that arrived rather than the \
             one the host is about to wait on for ever"
        );
    }

    #[test]
    fn a_window_that_starts_late_is_not_a_gap() {
        let app = app_with(&[100, 101, 102]);
        assert_eq!(
            tracker_gap::<u8>(&app),
            None,
            "ticks are pruned from the front once simulated, so a late start is housekeeping"
        );
    }
}
