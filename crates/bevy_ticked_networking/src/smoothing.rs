//! Hiding a correction from the eye without hiding it from the simulation.
//!
//! A snapshot that disagrees with the prediction moves a predicted body, and the renderer
//! shows the move as a jump. The simulation must take the correction whole — that is the
//! point — but the eye does not have to: the visual can carry the *difference* as an offset
//! that decays over a few frames, so the body slides to where it belongs instead of blinking
//! there. Two games shipped this (`rollback_smoothing.rs`, `smoothing.rs`, 159 and 261
//! lines); this is that, once.
//!
//! The offset is applied in `PostUpdate` onto the rendered transform, after the tick
//! interpolation blend, and undone in [`TickedSystems::Restore`] so the simulation never
//! integrates from it. With [`SmoothingTarget::Child`] it goes on a visual child instead and
//! the simulated transform is never touched at all.
//!
//! Exempt: the local player's own entities (a correction to what you are controlling should
//! be felt, and smoothing it makes input feel late), anything with [`NoCorrectionSmoothing`],
//! and the initial sync (a body arriving from nowhere has no old position worth sliding from).

use bevy::prelude::*;
use bevy_ticked::{
    TickedLoop, TickedSystems, interpolation::TickedInterpolationSet, registry::TickedComponent,
    tracked_entity::TickTrackedEntity, world_actions::WorldActions,
};
use std::collections::HashMap;

use crate::client::{AppliedSnapshotTick, ClientSet, LocalClientPlayer, SnapshotApplied};
use crate::replication::Owner;

/// Where the smoothing offset is written.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SmoothingTarget {
    /// The entity's own `Transform`, undone before every tick.
    #[default]
    Self_,
    /// A visual child's `Transform`, which the simulation never reads.
    Child(Entity),
}

/// Smooth this entity's corrections. Add [`TickedSmoothingPlugin`].
#[derive(Component, Clone, Copy, Debug, PartialEq)]
pub struct CorrectionSmoothing {
    /// How fast the offset decays, per second: the offset is multiplied by `exp(-rate * dt)`
    /// each frame. `12` closes most of a correction in a quarter of a second.
    pub decay_rate: f32,
    /// A correction larger than this is not smoothed: a respawn or a teleport is meant to be
    /// seen, and sliding across the map to it would look worse than the jump.
    pub max_offset: f32,
    /// The same for rotation, in radians.
    pub max_angle: f32,
    pub apply_to: SmoothingTarget,
}

impl Default for CorrectionSmoothing {
    fn default() -> Self {
        Self {
            decay_rate: 12.0,
            max_offset: 2.0,
            max_angle: 1.0,
            apply_to: SmoothingTarget::Self_,
        }
    }
}

/// Never smooth this entity, whatever it carries.
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct NoCorrectionSmoothing;

/// The offset currently hiding a correction, and what was last written so it can be undone.
#[derive(Component, Clone, Copy, Debug)]
pub struct SmoothingOffset {
    pub translation: Vec3,
    pub rotation: Quat,
    applied_translation: Vec3,
    applied_rotation: Quat,
}

impl Default for SmoothingOffset {
    fn default() -> Self {
        Self {
            translation: Vec3::ZERO,
            rotation: Quat::IDENTITY,
            applied_translation: Vec3::ZERO,
            applied_rotation: Quat::IDENTITY,
        }
    }
}

/// What the corrections have been like, for an overlay or a test.
#[derive(Resource, Debug, Clone, Copy, Default, PartialEq)]
pub struct CorrectionStats {
    /// Corrections that moved a smoothed entity at all.
    pub corrections: u64,
    /// Distance those moved, summed.
    pub total_offset: f32,
    pub max_offset: f32,
    pub last_offset: f32,
    /// Corrections too large to smooth, shown as jumps.
    pub snapped: u64,
}

impl CorrectionStats {
    pub fn reset(&mut self) {
        *self = Self::default();
    }
}

pub struct TickedSmoothingPlugin;

impl Plugin for TickedSmoothingPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<CorrectionStats>()
            .init_resource::<BeforeSnapshotTransforms>()
            .add_systems(
                TickedLoop,
                (
                    undo_offsets.in_set(TickedSystems::Restore),
                    remember_before.in_set(ClientSet::BeforeSnapshot),
                    absorb_corrections.in_set(ClientSet::AfterSnapshot),
                ),
            )
            .add_systems(
                PostUpdate,
                (decay_offsets, apply_offsets)
                    .chain()
                    .after(TickedInterpolationSet)
                    .before(TransformSystems::Propagate),
            );
    }
}

/// Where every smoothed entity was before this pass's snapshot, keyed by tracked id.
#[derive(Default)]
struct Before(HashMap<u64, Transform>);

fn remember_before(
    bodies: Query<(&TickTrackedEntity, &Transform), With<CorrectionSmoothing>>,
    mut before: Local<Before>,
    mut out: ResMut<BeforeSnapshotTransforms>,
) {
    before.0.clear();
    for (tracked, transform) in &bodies {
        before.0.insert(tracked.0, *transform);
    }
    out.0 = std::mem::take(&mut before.0);
}

#[derive(Resource, Default)]
struct BeforeSnapshotTransforms(HashMap<u64, Transform>);

fn absorb_corrections(
    mut bodies: Query<(
        Entity,
        &TickTrackedEntity,
        &Transform,
        &CorrectionSmoothing,
        Option<&Owner>,
        Option<&mut SmoothingOffset>,
        Has<NoCorrectionSmoothing>,
    )>,
    before: Res<BeforeSnapshotTransforms>,
    mut applied: MessageReader<SnapshotApplied>,
    local: Option<Res<LocalClientPlayer>>,
    mut stats: ResMut<CorrectionStats>,
    mut commands: Commands,
) {
    let mut any = false;
    let mut first = false;
    for message in applied.read() {
        any = true;
        first |= message.first;
    }
    if !any || first {
        return;
    }
    for (entity, tracked, transform, smoothing, owner, offset, exempt) in &mut bodies {
        if exempt {
            continue;
        }
        if let (Some(local), Some(owner)) = (&local, owner)
            && owner.0 == local.0
        {
            continue;
        }
        let Some(was) = before.0.get(&tracked.0) else {
            continue;
        };
        let jump = was.translation - transform.translation;
        let turn = was.rotation * transform.rotation.inverse();
        let distance = jump.length();
        if distance < 1e-6 && turn.angle_between(Quat::IDENTITY) < 1e-6 {
            continue;
        }
        stats.corrections += 1;
        stats.total_offset += distance;
        stats.max_offset = stats.max_offset.max(distance);
        stats.last_offset = distance;
        if distance > smoothing.max_offset
            || turn.angle_between(Quat::IDENTITY) > smoothing.max_angle
        {
            stats.snapped += 1;
            if let Some(mut offset) = offset {
                offset.translation = Vec3::ZERO;
                offset.rotation = Quat::IDENTITY;
            }
            continue;
        }
        match offset {
            Some(mut offset) => {
                offset.translation += jump;
                offset.rotation = turn * offset.rotation;
            }
            None => {
                commands.entity(entity).try_insert(SmoothingOffset {
                    translation: jump,
                    rotation: turn,
                    applied_translation: Vec3::ZERO,
                    applied_rotation: Quat::IDENTITY,
                });
            }
        }
    }
}

fn decay_offsets(
    time: Res<Time>,
    mut offsets: Query<(&CorrectionSmoothing, &mut SmoothingOffset)>,
) {
    let dt = time.delta_secs();
    for (smoothing, mut offset) in &mut offsets {
        let keep = (-smoothing.decay_rate * dt).exp();
        offset.translation *= keep;
        offset.rotation = Quat::IDENTITY.slerp(offset.rotation, keep);
        if offset.translation.length_squared() < 1e-8
            && offset.rotation.angle_between(Quat::IDENTITY) < 1e-4
        {
            offset.translation = Vec3::ZERO;
            offset.rotation = Quat::IDENTITY;
        }
    }
}

fn apply_offsets(
    mut offsets: Query<(Entity, &CorrectionSmoothing, &mut SmoothingOffset)>,
    mut transforms: Query<&mut Transform>,
) {
    for (entity, smoothing, mut offset) in &mut offsets {
        let target = match smoothing.apply_to {
            SmoothingTarget::Self_ => entity,
            SmoothingTarget::Child(child) => child,
        };
        let Ok(mut transform) = transforms.get_mut(target) else {
            continue;
        };
        match smoothing.apply_to {
            SmoothingTarget::Self_ => {
                transform.translation += offset.translation;
                transform.rotation = offset.rotation * transform.rotation;
                offset.applied_translation = offset.translation;
                offset.applied_rotation = offset.rotation;
            }
            SmoothingTarget::Child(_) => {
                transform.translation = offset.translation;
                transform.rotation = offset.rotation;
            }
        }
    }
}

/// Take back what `apply_offsets` wrote onto a simulated transform, before the tick reads it.
///
/// An entity with `TickedInterpolation` is put at the simulation's own value, which is what
/// the interpolation plugin's restore does too, so the two are idempotent whichever runs first
/// inside `Restore`. One without it gets the offset subtracted.
fn undo_offsets(
    mut offsets: Query<(
        &CorrectionSmoothing,
        &mut SmoothingOffset,
        &mut Transform,
        Option<&bevy_ticked::interpolation::TickedInterpolation>,
    )>,
) {
    for (smoothing, mut offset, mut transform, interpolation) in &mut offsets {
        if smoothing.apply_to != SmoothingTarget::Self_ {
            continue;
        }
        if offset.applied_translation == Vec3::ZERO && offset.applied_rotation == Quat::IDENTITY {
            continue;
        }
        match interpolation.and_then(|i| i.current()) {
            Some(current) => *transform = current,
            None => {
                transform.translation -= offset.applied_translation;
                transform.rotation = offset.applied_rotation.inverse() * transform.rotation;
            }
        }
        offset.applied_translation = Vec3::ZERO;
        offset.applied_rotation = Quat::IDENTITY;
    }
}

// ── measuring ────────────────────────────────────────────────────────────────

/// How far the authority's state at the snapshot tick was from what this client had predicted
/// for that tick, per snapshot. The prediction error, before any replay.
#[derive(Resource, Debug, Clone, Copy, Default, PartialEq)]
pub struct PredictionError {
    pub samples: u64,
    pub total: f32,
    pub max: f32,
    pub last: f32,
}

impl PredictionError {
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    pub fn mean(&self) -> f32 {
        if self.samples == 0 {
            0.0
        } else {
            self.total / self.samples as f32
        }
    }
}

/// Register a measurement of the prediction error on `T`: for every snapshot, `distance`
/// between what this client had captured for the snapshot's tick and what the authority sent,
/// summed over the entities the snapshot named and the client had, into [`PredictionError`].
///
/// Lifted from two games that measured it by hand around their rollback. Both wanted a number
/// a test could assert on, and both found the same thing: most of the error was remote bodies
/// simulated with no input, which [`ReplicationMode::Interpolated`] removed.
///
/// [`ReplicationMode::Interpolated`]: crate::replication::ReplicationMode::Interpolated
pub fn measure_prediction<T: TickedComponent + serde::de::DeserializeOwned>(
    app: &mut App,
    distance: fn(&T, &T) -> f32,
) {
    app.init_resource::<PredictionError>()
        .insert_resource(PredictionDistance::<T>(distance))
        .add_systems(
            TickedLoop,
            (
                sample_prediction_before::<T>.in_set(ClientSet::BeforeSnapshot),
                sample_prediction_after::<T>.in_set(ClientSet::AfterSnapshot),
            ),
        );
}

#[derive(Resource)]
struct PredictionDistance<T>(fn(&T, &T) -> f32);

/// Prediction at the pending snapshot's tick, captured before the snapshot overwrites it.
#[derive(Resource, Default)]
struct PredictedAt<T: TickedComponent>(Option<(u64, HashMap<u64, T>)>);

fn sample_prediction_before<T: TickedComponent>(world: &mut World) {
    let pending_tick = world
        .get_resource::<crate::client::PendingSnapshotTick>()
        .and_then(|p| p.0);
    let Some(tick) = pending_tick else {
        world.insert_resource(PredictedAt::<T>(None));
        return;
    };
    let predicted = world
        .resource::<WorldActions<T>>()
        .at_tick(tick)
        .map(|state| {
            state
                .iter()
                .map(|(id, v)| (*id, v.clone()))
                .collect::<HashMap<_, _>>()
        });
    world.insert_resource(PredictedAt::<T>(predicted.map(|p| (tick, p))));
}

fn sample_prediction_after<T: TickedComponent>(world: &mut World) {
    let Some((tick, predicted)) = world
        .get_resource_mut::<PredictedAt<T>>()
        .and_then(|mut p| p.0.take())
    else {
        return;
    };
    let applied = world.resource::<AppliedSnapshotTick>().0;
    if applied != Some(tick) {
        return;
    }
    let distance = world.resource::<PredictionDistance<T>>().0;
    let authoritative = world.resource::<WorldActions<T>>();
    let Some(truth) = authoritative.at_tick(tick) else {
        return;
    };
    let mut total = 0.0;
    for (id, mine) in &predicted {
        if let Some(theirs) = truth.get(id) {
            total += distance(mine, theirs);
        }
    }
    let mut error = world.resource_mut::<PredictionError>();
    error.samples += 1;
    error.total += total;
    error.max = error.max.max(total);
    error.last = total;
}
