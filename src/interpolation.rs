//! Sub-tick interpolation.
//!
//! The simulation only advances on ticks, but rendering runs every frame. To
//! avoid visible stutter, a visual that tracks a simulated value blends between
//! its previous- and current-tick states using the fraction of the way through
//! the pending tick.
//!
//! That fraction comes from [`Time<Ticked>`], the crate's own clock, not from
//! `Time<Fixed>`. Under [`TickSource::FixedUpdate`] the two are mirrored and the
//! result is identical; under any other source `Time<Fixed>` is unrelated to the
//! tick rate, and reading it would blend against the wrong clock entirely.
//!
//! [`TickInterpolation`] bundles [`CurrentTick`] with that always-clamped
//! fraction behind one consistent API, so interpolated visuals can't drift out
//! of sync with each other or re-derive the idiom by hand.
//!
//! [`TickSource::FixedUpdate`]: crate::TickSource::FixedUpdate

use bevy::ecs::system::SystemParam;
use bevy::prelude::*;

use crate::tick::CurrentTick;
use crate::time::{Ticked, TickedTime};

/// Read-only access to the current sub-tick interpolation state.
///
/// Bundles the current simulation tick with the fractional progress through the
/// next tick. Use it in any `Update`/`PostUpdate` rendering system that needs to
/// smooth a tick-driven value across frames.
#[derive(SystemParam)]
pub struct TickInterpolation<'w> {
    current_tick: Res<'w, CurrentTick>,
    ticked_time: Res<'w, Time<Ticked>>,
}

impl TickInterpolation<'_> {
    /// Fraction in `[0, 1]` of the way from the last completed tick to the next.
    ///
    /// This is the blend factor for a two-point interpolation between a value's
    /// previous-tick and current-tick states.
    pub fn fraction(&self) -> f32 {
        self.ticked_time.overstep_fraction()
    }

    /// The current simulation tick.
    pub fn current_tick(&self) -> u64 {
        self.current_tick.0
    }

    /// Continuous number of ticks elapsed since `tick`, including the current
    /// sub-tick [`fraction`](Self::fraction).
    ///
    /// Saturates at the fractional part for ticks at or in the future, so a
    /// freshly stamped event animates from zero rather than jumping.
    pub fn ticks_since(&self, tick: u64) -> f32 {
        self.current_tick.0.saturating_sub(tick) as f32 + self.fraction()
    }
}

/// Blend a tick-driven `Transform` across frames, from the tick clock.
///
/// # Why this is here rather than borrowed
///
/// `bevy_transform_interpolation` — which avian installs as
/// `PhysicsInterpolationPlugin` — takes its blend factor from
/// `Res<Time<Fixed>>::overstep_fraction()` and installs its systems into
/// `RunFixedMainLoop`, both hardcoded. avian itself takes a schedule, so physics
/// can advance somewhere other than Bevy's fixed loop; its interpolation cannot
/// follow.
///
/// Under [`TickSource::Hz`](crate::TickSource::Hz) that is not a small phase
/// error. Ticks run in `RunTickedLoop`, which is inserted *after*
/// `RunFixedMainLoop`, so the easing runs **before** the tick that produces the
/// states it is meant to blend, and takes its fraction from an accumulator with no
/// relationship to how far through the current tick the simulation actually is: a
/// stale pair blended with an uncorrelated alpha. It reads as position jitter.
///
/// The workaround both consumers found was to pin the tick source to
/// `FixedUpdate` — which costs [`TickRateDilation`](crate::time::TickRateDilation),
/// the only mechanism for steering a client's prediction lead without visibly
/// adding or dropping a whole tick — or to hand-write the blend. This does the
/// hand-written version once, from [`Time<Ticked>`](crate::time::Ticked), which is
/// correct under every tick source.
///
/// # Using it
///
/// Add [`TickedInterpolationPlugin`] and put [`TickedInterpolation`] on anything
/// whose `Transform` the simulation writes. Where avian is also present, give those
/// entities `NoTransformEasing` so only one author remains.
pub struct TickedInterpolationPlugin;

/// Interpolate this entity's `Transform` between its last two tick states.
///
/// The states are recorded here rather than read from
/// [`WorldActions`](crate::world_actions::WorldActions) on purpose: a visual
/// transform is not necessarily a registered component, and an entity that is
/// merely *drawn* from the simulation should not have to be replicated to be
/// smooth.
///
/// # The blend never reaches the simulation
///
/// The blended value is written into `Transform` for the renderer, and put back before the
/// next tick reads it ([`TickedSystems::Restore`](crate::TickedSystems::Restore)). It used to
/// stay: the tick then integrated from a transform that was `fraction` of the way back toward
/// the previous tick, and a body meant to cross 99 units in 99 ticks crossed 75. Every consumer
/// that measured it read it as "physics feels floaty under Hz" and pinned the tick source to
/// `FixedUpdate` to make it go away.
#[derive(Component, Debug, Clone, Copy, Default)]
pub struct TickedInterpolation {
    previous: Option<Transform>,
    current: Option<Transform>,
}

impl TickedInterpolation {
    /// The transform as the simulation last left it: the true state, not a blend.
    pub fn current(&self) -> Option<Transform> {
        self.current
    }

    /// The blend of the last two tick states at `fraction` through the tick.
    ///
    /// `None` until two ticks have been seen, so the first frame after a spawn
    /// draws the entity where it is rather than at the origin.
    pub fn sample(&self, fraction: f32) -> Option<Transform> {
        let (previous, current) = (self.previous?, self.current?);
        Some(Transform {
            translation: previous.translation.lerp(current.translation, fraction),
            rotation: previous.rotation.slerp(current.rotation, fraction),
            scale: previous.scale.lerp(current.scale, fraction),
        })
    }
}

/// Ordering hook, so a consumer can write its own offsets after the blend.
#[derive(SystemSet, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TickedInterpolationSet;

impl Plugin for TickedInterpolationPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(
            crate::TickedLoop,
            (
                restore_simulation_transform.in_set(crate::TickedSystems::Restore),
                record_tick_states.in_set(crate::TickedSystems::PostTick),
            ),
        )
        .add_systems(
            PostUpdate,
            apply_tick_interpolation
                .in_set(TickedInterpolationSet)
                .before(TransformSystems::Propagate),
        );
    }
}

/// After each tick, the state it produced becomes "current" and the old current
/// becomes "previous".
///
/// In `PostTick` rather than in `Update`: a replayed tick has to shift the pair
/// too, or a client that rolls back blends toward a state it has already
/// discarded.
fn record_tick_states(mut bodies: Query<(&Transform, &mut TickedInterpolation)>) {
    for (transform, mut interpolation) in &mut bodies {
        interpolation.previous = interpolation.current;
        interpolation.current = Some(*transform);
    }
}

/// Before anything in the loop reads a transform, put the true one back.
///
/// `GlobalTransform` too, for entities with no parent: propagation only runs in `PostUpdate`,
/// so between it and the next tick the global transform is the blend, and a physics engine or
/// a raycast reading it inside the tick would see the presentation value.
fn restore_simulation_transform(
    mut bodies: Query<(
        &mut Transform,
        Option<&mut GlobalTransform>,
        Has<ChildOf>,
        &TickedInterpolation,
    )>,
) {
    for (mut transform, global, has_parent, interpolation) in &mut bodies {
        let Some(current) = interpolation.current else {
            continue;
        };
        if *transform != current {
            *transform = current;
            if let Some(mut global) = global
                && !has_parent
            {
                *global = GlobalTransform::from(current);
            }
        }
    }
}

/// Each frame, write the blend of the last two tick states.
fn apply_tick_interpolation(
    tick: TickInterpolation,
    mut bodies: Query<(&mut Transform, &TickedInterpolation)>,
) {
    let fraction = tick.fraction();
    for (mut transform, interpolation) in &mut bodies {
        if let Some(blended) = interpolation.sample(fraction) {
            *transform = blended;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(x: f32) -> Transform {
        Transform::from_xyz(x, 0.0, 0.0)
    }

    #[test]
    fn the_first_tick_alone_is_not_enough_to_blend() {
        let mut state = TickedInterpolation::default();
        assert!(state.sample(0.5).is_none(), "nothing to blend against yet");
        state.current = Some(at(1.0));
        assert!(
            state.sample(0.5).is_none(),
            "one state is a position, not an interval -- blending here would draw \
             a freshly spawned body sliding in from the origin"
        );
    }

    #[test]
    fn the_blend_spans_the_two_most_recent_ticks() {
        let state = TickedInterpolation {
            previous: Some(at(0.0)),
            current: Some(at(10.0)),
        };
        assert_eq!(state.sample(0.0).unwrap().translation.x, 0.0);
        assert_eq!(state.sample(1.0).unwrap().translation.x, 10.0);
        assert_eq!(state.sample(0.25).unwrap().translation.x, 2.5);
    }
}
