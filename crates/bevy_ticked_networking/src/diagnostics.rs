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
    /// Snapshots that reached the client role: decoded, past the handshake gate, and handed to
    /// the receiving observer. Everything below is a fate one of these met.
    pub snapshots_received: u64,
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
    /// Deltas dropped because their baseline was no longer held; a full body was asked for.
    pub dropped_unknown_baseline: u64,
    /// Packets that arrived as deltas and were rebuilt into full bodies.
    pub deltas_applied: u64,
    /// Rewinds: an excess lead given back in one step rather than dilated away, each the
    /// ticks this client predicted past a host that had stalled.
    pub snapped_back: u64,
    /// Snapshot packets that did not decode, counted by the transport bridge.
    pub dropped_undecodable: u64,
}

impl ReplayStats {
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// The counts accumulated since `earlier` was read. `last_replay_distance` is a reading, not
    /// a count, and is this one's.
    pub fn since(&self, earlier: &Self) -> Self {
        Self {
            snapshots_received: self
                .snapshots_received
                .saturating_sub(earlier.snapshots_received),
            snapshots_applied: self
                .snapshots_applied
                .saturating_sub(earlier.snapshots_applied),
            rollbacks: self.rollbacks.saturating_sub(earlier.rollbacks),
            ticks_replayed: self.ticks_replayed.saturating_sub(earlier.ticks_replayed),
            skipped_identical: self
                .skipped_identical
                .saturating_sub(earlier.skipped_identical),
            dropped_stale: self.dropped_stale.saturating_sub(earlier.dropped_stale),
            dropped_before_handshake: self
                .dropped_before_handshake
                .saturating_sub(earlier.dropped_before_handshake),
            last_replay_distance: self.last_replay_distance,
            dropped_unknown_baseline: self
                .dropped_unknown_baseline
                .saturating_sub(earlier.dropped_unknown_baseline),
            deltas_applied: self.deltas_applied.saturating_sub(earlier.deltas_applied),
            snapped_back: self.snapped_back.saturating_sub(earlier.snapped_back),
            dropped_undecodable: self
                .dropped_undecodable
                .saturating_sub(earlier.dropped_undecodable),
        }
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
    /// Packets larger than [`SNAPSHOT_ADVISORY_BYTES`]: over a datagram's comfortable size, and
    /// on some links over the size that arrives at all. Warned once.
    pub oversize: u64,
    /// Packets sent as deltas.
    pub deltas: u64,
    /// Packets sent as full bodies (joins, keyframes, nacks, evicted baselines).
    pub keyframes: u64,
}

/// A snapshot above this many bytes is a warning. Below the common path MTU with room for
/// the transport's framing; above it a datagram may be fragmented and a lost fragment loses
/// the whole thing.
pub const SNAPSHOT_ADVISORY_BYTES: usize = 1200;

impl SnapshotStats {
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// Fold one encoded packet's size in.
    pub fn record_bytes(&mut self, bytes: usize) {
        self.bytes += bytes as u64;
        self.max_bytes = self.max_bytes.max(bytes);
        self.last_bytes = bytes;
        if bytes > SNAPSHOT_ADVISORY_BYTES {
            if self.oversize == 0 {
                warn!(
                    "a snapshot packet is {bytes} bytes, over the {SNAPSHOT_ADVISORY_BYTES}-byte \
                     advisory; a datagram this size may be fragmented and lost whole. Register \
                     fewer types, quantise inside the simulation, or wait for delta \
                     replication (said once; counted from now on)"
                );
            }
            self.oversize += 1;
        }
    }
}

/// What the host has been receiving from clients.
///
/// Inserted by [`TickedServerPlugin`](crate::server::TickedServerPlugin).
#[derive(Resource, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct InputStats {
    /// Inputs accepted into the queue, redundant copies included.
    pub received: u64,
    /// Inputs that arrived for a tick the host had already run, redundant copies included: a
    /// copy of an input the host had on time counts here too.
    pub late: u64,
    /// Ticks of input the host received for the first time: each tick a client stamped,
    /// counted once however many redundant copies of it arrived.
    pub new_ticks: u64,
    /// Of [`new_ticks`](Self::new_ticks), the ones whose first copy came after the host had run
    /// the tick. Each is an input the host did not have when it needed it, so the share of
    /// these is the number a prediction lead exists to keep near zero.
    pub late_new_ticks: u64,
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
/// Each warns once, then counts. The first three are the ones a consumer's diagnostics document
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
    /// The host could not keep up with real time for long enough to pause the session: frames
    /// longer than the backlog `MaxTicksPerFrame` lets it run, one after another. Counted rather
    /// than only curtained, because the cause is the host's frame cost and not the network, and
    /// the symptom everybody else sees — a lead that keeps being taken back — names neither.
    pub host_behind_real_time: u32,
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

/// Corrections to the bodies this client controls: what its own player sees jump.
///
/// A body the client owns is predicted from its own inputs, so a correction to it means the
/// host ran something else — usually an input that reached it too late — and the player's own
/// character snaps. [`CorrectionSmoothing`](crate::smoothing::CorrectionSmoothing) leaves such a
/// body unsmoothed on purpose, so these are drawn exactly as counted.
///
/// Measured on every owned entity with a
/// [`TickedInterpolation`](bevy_ticked::interpolation::TickedInterpolation): its pose at the
/// current tick before a snapshot was applied, against the pose the presentation draws at the
/// same tick after the rollback and replay. Inserted by
/// [`TickedClientPlugin`](crate::client::TickedClientPlugin).
#[derive(Resource, Debug, Clone, Copy, PartialEq)]
pub struct OwnCorrectionStats {
    /// The smallest correction counted, in world units. Smaller ones are rounding rather than
    /// anything a player sees; set it to what one unit is worth in the game.
    pub threshold: f32,
    /// Corrections of at least [`threshold`](Self::threshold).
    pub corrections: u64,
    /// Of those, the ones that pushed the body back against the way it was moving: the snap a
    /// player feels as being pulled backwards.
    pub against_motion: u64,
    /// Distance of the counted corrections, summed.
    pub total_offset: f32,
    pub max_offset: f32,
    pub last_offset: f32,
}

impl Default for OwnCorrectionStats {
    fn default() -> Self {
        Self {
            threshold: 1.0,
            corrections: 0,
            against_motion: 0,
            total_offset: 0.0,
            max_offset: 0.0,
            last_offset: 0.0,
        }
    }
}

impl OwnCorrectionStats {
    /// Zero the counts, keeping the threshold.
    pub fn reset(&mut self) {
        *self = Self {
            threshold: self.threshold,
            ..Self::default()
        };
    }
}

/// The poses of this client's own bodies before a snapshot pass, and the tick they are for.
#[derive(Resource, Default)]
struct OwnPosesBefore {
    tick: Option<bevy_ticked::tick_types::Tick>,
    /// Entity, pose at `tick`, and the pose the tick before it.
    poses: Vec<(Entity, Transform, Option<Transform>)>,
}

pub(crate) fn install_own_corrections(app: &mut App) {
    use crate::client::ClientSet;
    app.init_resource::<OwnCorrectionStats>()
        .init_resource::<OwnPosesBefore>()
        .add_systems(
            bevy_ticked::TickedLoop,
            (
                remember_own_poses.in_set(ClientSet::BeforeSnapshot),
                measure_own_corrections.in_set(ClientSet::AfterSnapshot),
            ),
        );
}

fn remember_own_poses(
    tick: Res<bevy_ticked::tick::CurrentTick>,
    local: Option<Res<crate::client::LocalClientPlayer>>,
    bodies: Query<(
        Entity,
        &crate::replication::Owner,
        &bevy_ticked::interpolation::TickedInterpolation,
    )>,
    mut before: ResMut<OwnPosesBefore>,
) {
    before.poses.clear();
    before.tick = None;
    let Some(local) = local else {
        return;
    };
    before.tick = Some(tick.0);
    for (entity, owner, interpolation) in &bodies {
        if owner.0 != local.0 {
            continue;
        }
        if let Some(current) = interpolation.current() {
            before
                .poses
                .push((entity, current, interpolation.previous()));
        }
    }
}

fn measure_own_corrections(
    tick: Res<bevy_ticked::tick::CurrentTick>,
    mut applied: MessageReader<crate::client::SnapshotApplied>,
    bodies: Query<&Transform>,
    before: Res<OwnPosesBefore>,
    mut stats: ResMut<OwnCorrectionStats>,
) {
    let mut any = false;
    let mut first = false;
    for message in applied.read() {
        any = true;
        first |= message.first;
    }
    // The initial sync is the world arriving, not a correction; and a replay too long for one
    // frame leaves the clock short of the tick the poses were taken at, where they cannot be
    // compared.
    if !any || first || before.tick != Some(tick.0) {
        return;
    }
    for &(entity, was, previous) in &before.poses {
        // The `Transform` the snapshot's presentation pass wrote from the replayed state: a
        // replay re-runs the simulation, not the per-tick interpolation record.
        let Ok(now) = bodies.get(entity) else {
            continue;
        };
        let jump = now.translation - was.translation;
        let distance = jump.length();
        if distance < stats.threshold {
            continue;
        }
        stats.corrections += 1;
        stats.total_offset += distance;
        stats.max_offset = stats.max_offset.max(distance);
        stats.last_offset = distance;
        let motion = previous.map_or(Vec3::ZERO, |previous| {
            was.translation - previous.translation
        });
        if jump.dot(motion) < 0.0 {
            stats.against_motion += 1;
        }
    }
}
