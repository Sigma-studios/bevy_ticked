//! The numbers a networked session has to be able to show.
//!
//! Every consumer of this crate wrote some of these: a replay counter bracketing `TickedLoop`, a
//! snapshot byte counter at the bridge, a "corrections per second" fold for the F3 overlay. Three
//! games, three implementations, none shared — and the harness that tests this crate needs the
//! same numbers, or it measures something the player never sees.
//!
//! So they live here, always compiled, counted inline where the event happens: a rollback is
//! counted in the code that rolls back, a stale snapshot in the code that drops it. A game shows
//! them on an overlay; a test asserts on them; the two agree because they are the same
//! resource.
//!
//! Rules, learned the hard way by the games that wrote the originals:
//! - **Loud once, then quiet.** A [`HealthWarnings`] entry warns the first time and counts after
//!   that. A warning at frame rate says nothing.
//! - **Name both sides.** A mismatch message that says "the snapshot's tick" and "the oldest tick
//!   in history" is one somebody can act on.
//! - **Measure the tick, not the frame.** See [`TickCost`](bevy_ticked::diagnostics::TickCost).

use bevy::prelude::*;

/// What the client's rollback has been doing.
///
/// Inserted by [`TickedClientPlugin`](crate::client::TickedClientPlugin). Cumulative; reset with
/// [`reset`](Self::reset) to measure a window.
#[derive(Resource, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReplayStats {
    /// Snapshots applied to the world.
    pub snapshots_applied: u64,
    /// Times the world was restored to a snapshot and predicted ticks replayed.
    pub rollbacks: u64,
    /// Ticks re-simulated by those rollbacks, in total. Divided by frames, this is the
    /// "seven simulation runs per frame" number.
    pub ticks_replayed: u64,
    /// Snapshots that agreed with the prediction and cost no rollback. Zero until the comparison
    /// fast path lands.
    pub skipped_identical: u64,
    /// Snapshots dropped at the door because a newer one had already been applied or was
    /// waiting.
    pub dropped_stale: u64,
    /// Snapshots dropped because this peer had not taken the client role yet (later: because
    /// the registry handshake had not matched yet).
    pub dropped_before_handshake: u64,
    /// `current_tick - snapshot_tick` at the last applied snapshot: the replay distance.
    pub last_replay_distance: i64,
}

impl ReplayStats {
    pub fn reset(&mut self) {
        *self = Self::default();
    }
}

/// What the host has been sending.
///
/// Inserted by [`TickedServerPlugin`](crate::server::TickedServerPlugin). `sent` counts
/// broadcasts; the byte fields are filled by whichever transport bridge serialises the
/// snapshot, and stay zero without one.
#[derive(Resource, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SnapshotStats {
    /// Snapshot broadcasts built.
    pub sent: u64,
    /// Bytes of serialised snapshot, summed over every recipient.
    pub bytes: u64,
    /// The largest single serialised snapshot.
    pub max_bytes: usize,
    /// The most recent one.
    pub last_bytes: usize,
}

impl SnapshotStats {
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// Fold one serialised snapshot's size in. Called by a bridge.
    pub fn record_bytes(&mut self, bytes: usize) {
        self.bytes += bytes as u64;
        self.max_bytes = self.max_bytes.max(bytes);
        self.last_bytes = bytes;
    }
}

/// What the host has been receiving from clients.
///
/// Inserted by [`TickedServerPlugin`](crate::server::TickedServerPlugin).
#[derive(Resource, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct InputStats {
    /// Inputs accepted into the queue, redundant copies included.
    pub received: u64,
    /// Inputs that arrived for a tick the host had already run.
    pub late: u64,
    /// Inputs refused because their tick was outside the window the host keeps: older than
    /// `HistoryBufferTicks` behind the host, or further ahead than `MAX_INPUT_LEAD_TICKS`. Not
    /// counted in `received`, and never seen by the queue or the margins.
    pub dropped_out_of_window: u64,
}

impl InputStats {
    pub fn reset(&mut self) {
        *self = Self::default();
    }
}

/// Things that should not happen, counted when they do.
///
/// Each warns once, then counts. The three here are the ones a consumer's diagnostics document
/// asked for by name, because each had cost them a session to find by hand.
#[derive(Resource, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HealthWarnings {
    /// A client's tracked-entity counter advanced without a snapshot doing it: the client
    /// minted an id, which only the authority may (until predicted spawns land).
    pub client_minted_tracked_id: u32,
    /// A snapshot named the same tracked id under two entities' worth of components.
    pub duplicate_ids_in_snapshot: u32,
    /// A snapshot arrived for a tick older than the oldest tick the client still has history
    /// for, so the replay from it could not restore rollback-only state.
    pub snapshot_older_than_history: u32,
}

impl HealthWarnings {
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// Increment `counter`, warning with `message` the first time only.
    pub(crate) fn raise(counter: &mut u32, message: impl FnOnce() -> String) {
        if *counter == 0 {
            warn!("{} (said once; counted from now on)", message());
        }
        *counter = counter.saturating_add(1);
    }
}
