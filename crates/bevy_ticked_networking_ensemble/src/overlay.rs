//! Publishes the ticked counters to bevy_ensemble's net-debug overlay.
//!
//! Enabled by the `overlay` feature (on by default, like `bevy_ensemble/netdebug` that it pulls
//! in). Nothing here is read back by the library: the lines exist so a player with the overlay
//! open sees the simulation's numbers next to the transport's, which is where the audit found
//! its "seven replays per frame" and "1340 bytes per tick". Keys are prefixed `ticked.` so a
//! game's own lines never collide with them.

use bevy::prelude::*;
use bevy_ensemble::NetDebugExtras;
use bevy_ticked::diagnostics::TickCost;
use bevy_ticked_networking::diagnostics::{HealthWarnings, InputStats, ReplayStats, SnapshotStats};

pub(crate) struct TickedOverlayPlugin;

impl Plugin for TickedOverlayPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(PostUpdate, publish_ticked_lines);
    }
}

/// Snapshot of the counters at the last frame, so the overlay shows rates, not just totals.
#[derive(Default)]
struct LastFrame {
    ticks: u64,
    rollbacks: u64,
    ticks_replayed: u64,
    snapshots: u64,
    inputs: u64,
    at: f64,
}

fn publish_ticked_lines(
    extras: Option<ResMut<NetDebugExtras>>,
    time: Res<Time<Real>>,
    cost: Option<Res<TickCost>>,
    replay: Option<Res<ReplayStats>>,
    snapshots: Option<Res<SnapshotStats>>,
    inputs: Option<Res<InputStats>>,
    health: Option<Res<HealthWarnings>>,
    mut last: Local<LastFrame>,
) {
    let Some(mut extras) = extras else { return };
    let now = time.elapsed_secs_f64();
    let dt = (now - last.at).max(1e-6);
    // Rates are refreshed twice a second; an overlay that flickers at frame rate is unreadable.
    let refresh = now - last.at >= 0.5;

    if let Some(cost) = cost {
        extras.set(
            "ticked.tick",
            format!(
                "tick: {:.2} ms mean, {:.2} ms worst, {} run",
                cost.mean().as_secs_f64() * 1e3,
                cost.worst.as_secs_f64() * 1e3,
                cost.ticks
            ),
        );
        if refresh {
            let per_s = (cost.ticks - last.ticks) as f64 / dt;
            extras.set("ticked.rate", format!("ticks/s: {per_s:.1}"));
            last.ticks = cost.ticks;
        }
    }
    if let Some(replay) = replay {
        if refresh {
            let rollbacks_per_s = (replay.rollbacks - last.rollbacks) as f64 / dt;
            let ticks_per_s = (replay.ticks_replayed - last.ticks_replayed) as f64 / dt;
            extras.set(
                "ticked.replay",
                format!(
                    "replay: {rollbacks_per_s:.1} rollbacks/s, {ticks_per_s:.0} ticks/s, \
                     distance {}, {} identical, {} stale",
                    replay.last_replay_distance, replay.skipped_identical, replay.dropped_stale
                ),
            );
            last.rollbacks = replay.rollbacks;
            last.ticks_replayed = replay.ticks_replayed;
        }
    }
    if let Some(snapshots) = snapshots {
        if refresh {
            let per_s = (snapshots.sent - last.snapshots) as f64 / dt;
            extras.set(
                "ticked.snapshot",
                format!(
                    "snapshot: {per_s:.1}/s, {} B last, {} B max, {} oversize",
                    snapshots.last_bytes, snapshots.max_bytes, snapshots.oversize
                ),
            );
            last.snapshots = snapshots.sent;
        }
    }
    if let Some(inputs) = inputs {
        if refresh {
            let per_s = (inputs.received - last.inputs) as f64 / dt;
            extras.set(
                "ticked.input",
                format!(
                    "input: {per_s:.1}/s, {} late, {} out of window",
                    inputs.late, inputs.dropped_out_of_window
                ),
            );
            last.inputs = inputs.received;
        }
    }
    if let Some(health) = health {
        let total = health.client_minted_tracked_id
            + health.duplicate_ids_in_snapshot
            + health.snapshot_older_than_history;
        if total > 0 {
            extras.set(
                "ticked.health",
                format!(
                    "HEALTH: {} client-minted ids, {} duplicate ids, {} snapshots older than history",
                    health.client_minted_tracked_id,
                    health.duplicate_ids_in_snapshot,
                    health.snapshot_older_than_history
                ),
            );
        }
    }
    if refresh {
        last.at = now;
    }
}
