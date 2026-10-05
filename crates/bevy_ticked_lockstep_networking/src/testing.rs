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
use bevy_ensemble::LobbyParticipant;
use bevy_ticked::prelude::{CurrentTick, TickHolds};
use bevy_ticked::tick_types::{Tick, Ticks};

use crate::{
    ActionTracker, LastScheduledTick, LocalPendingActions, LockstepAction, LockstepConfig,
    LockstepLobbyParticipant,
};

/// The tick this peer has simulated up to.
pub fn current_tick(app: &App) -> Tick {
    app.world().resource::<CurrentTick>().0
}

/// The tick from which this peer's roster requires `player_uuid`'s actions, or `None` if that
/// player is not (yet) a lockstep participant here.
///
/// On the host it is the number the joiner's grace window is measured from, which is what a
/// test of the window asserts about; a second `ClientLoaded` used to move it.
pub fn participant_joined_at(app: &App, player_uuid: u128) -> Option<Tick> {
    let world = app.world();
    // `try_query` rather than `query`: it needs no `&mut World`, so this composes with a
    // network's `run_until`, and a world that has never seen the component has nobody on it.
    let mut query = world.try_query::<(&LobbyParticipant, &LockstepLobbyParticipant)>()?;
    query
        .iter(world)
        .find(|(participant, _)| participant.player_uuid == player_uuid)
        .map(|(_, lockstep)| lockstep.joined_at_tick)
}

/// Whether this peer's simulation is currently held.
///
/// On a client that usually means it is waiting for an authoritative tick, which is the normal
/// resting state of a peer that has caught up — not a fault by itself.
pub fn is_paused(app: &App) -> bool {
    app.world().resource::<TickHolds>().is_held()
}

/// What the client-side buffer has settled on, in ticks.
pub fn client_tick_buffer(app: &App) -> Ticks {
    app.world().resource::<LockstepConfig>().client_tick_buffer
}

/// What the host-side buffer has settled on, in ticks.
pub fn host_tick_buffer(app: &App) -> Ticks {
    app.world().resource::<LockstepConfig>().host_tick_buffer
}

/// The newest tick this peer has scheduled actions for.
///
/// Sits `buffer` ahead of its current tick. What matters is that the sequence leading up to it
/// has no holes — see [`tracker_gap`].
pub fn last_scheduled_tick(app: &App) -> Tick {
    app.world()
        .resource::<LastScheduledTick>()
        .0
        .unwrap_or(Tick::ZERO)
}

/// Which ticks this peer has any action data for, ascending.
pub fn tracked_ticks<A: LockstepAction>(app: &App) -> Vec<Tick> {
    let mut ticks: Vec<Tick> = app
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
pub fn tracker_gap<A: LockstepAction>(app: &App) -> Option<Tick> {
    tracked_ticks::<A>(app)
        .windows(2)
        .find(|pair| pair[1] != pair[0].next())
        .map(|pair| pair[0].next())
}

/// Every action recorded for `tick`, flattened across players in the tracker's own order.
pub fn actions_at<A: LockstepAction>(app: &App, tick: Tick) -> Vec<A> {
    app.world()
        .resource::<ActionTracker<A>>()
        .actions_for_tick(tick)
        .map(|players| players.values().flatten().cloned().collect())
        .unwrap_or_default()
}

/// Where this peer is in a host migration.
pub fn migration_state(app: &App) -> crate::LockstepMigration {
    app.world().resource::<crate::LockstepMigration>().clone()
}

/// The last tick this peer ruled or was told was ruled: past its clock on a host that took over
/// and has not caught up.
pub fn last_broadcast_tick(app: &App) -> Tick {
    app.world().resource::<crate::LastBroadcastTick>().0
}

/// The newest tick this peer holds a ruling for.
pub fn newest_ruled_tick<A: LockstepAction>(app: &App) -> Option<Tick> {
    app.world().resource::<ActionTracker<A>>().newest_tick()
}

/// The actions this client scheduled that no ruling has covered yet, by tick.
pub fn unruled_local_actions<A: LockstepAction>(app: &App) -> Vec<(Tick, Vec<A>)> {
    app.world()
        .resource::<crate::UnruledLocalActions<A>>()
        .0
        .iter()
        .map(|(tick, actions)| (*tick, actions.clone()))
        .collect()
}

/// Queue an action as though this peer's local input had produced it.
///
/// It is scheduled by the next flush — into the next tick on a host, `buffer` ticks ahead on a
/// client — exactly as a real one would be, so a test on a client that pushes an action and
/// then asserts about the very next tick is asserting about the wrong tick.
///
/// # Panics
///
/// If this peer has no `LocalPendingActions<A>`, which means `A` is not the peer's action type.
/// It used to do nothing, and an integer literal that defaulted to `i32` on a peer whose
/// actions were `u8` made a test pass by pushing nothing at all.
pub fn push_action<A: LockstepAction>(app: &mut App, action: A) {
    let Some(mut pending) = app.world_mut().get_resource_mut::<LocalPendingActions<A>>() else {
        panic!(
            "this peer has no LocalPendingActions<{}>: is that its action type?",
            std::any::type_name::<A>()
        );
    };
    pending.0.push(action);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app_with(ticks: &[u64]) -> App {
        let mut app = App::new();
        let mut tracker = ActionTracker::<u8>::default();
        for tick in ticks {
            tracker.ticks.entry(Tick(*tick)).or_default();
        }
        app.insert_resource(tracker);
        app
    }

    #[test]
    fn a_contiguous_sequence_has_no_gap() {
        let app = app_with(&[4, 5, 6, 7]);
        assert_eq!(tracker_gap::<u8>(&app), None);
        assert_eq!(tracked_ticks::<u8>(&app), [4, 5, 6, 7].map(Tick).to_vec());
    }

    #[test]
    fn the_gap_reported_is_the_first_missing_tick() {
        let app = app_with(&[4, 5, 9, 10]);
        assert_eq!(
            tracker_gap::<u8>(&app),
            Some(Tick(6)),
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
