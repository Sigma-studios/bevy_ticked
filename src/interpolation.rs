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
use crate::tick_types::Tick;
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
    pub fn current_tick(&self) -> crate::tick_types::Tick {
        self.current_tick.0
    }

    /// Continuous number of ticks elapsed since `tick`, including the current
    /// sub-tick [`fraction`](Self::fraction).
    ///
    /// Saturates at the fractional part for ticks at or in the future, so a
    /// freshly stamped event animates from zero rather than jumping.
    pub fn ticks_since(&self, tick: crate::tick_types::Tick) -> f32 {
        self.current_tick.0.since(tick).0 as f32 + self.fraction()
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
/// Add [`TickedInterpolationPlugin`] and put [`TickedInterpolation`] on anything whose
/// `Transform` is built once per tick by [`TickedPresentation`](crate::TickedPresentation). The
/// simulation never writes a `Transform` — the transform guard holds it to that in a debug
/// build — so the two last poses recorded here are what the presentation made of the last two
/// ticks, and the blend between them is the only other thing that writes the component.
///
/// A body that jumps — a portal, a respawn — carries [`Teleported`] stamped with the tick it
/// jumped on, and is drawn there rather than slid across the map.
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
/// It cannot: nothing in the tick reads a `Transform`. It used to — the tick integrated from the
/// component the renderer had last blended, a body meant to cross 99 units in 99 ticks crossed 75,
/// and a restore step put the true transform back before every tick to hide it. With the picture
/// built after the tick from simulated state, there is nothing to put back.
#[derive(Component, Debug, Clone, Copy, Default)]
pub struct TickedInterpolation {
    previous: Option<Transform>,
    current: Option<Transform>,
    /// The tick `current` was recorded at, so a [`Teleported`] stamp can be told apart from one
    /// that has already been honoured.
    recorded_at: Option<Tick>,
}

/// This entity jumped on this tick, and is not to be blended across the jump.
///
/// Simulated state, stamped by whatever moved the body — a portal, a respawn — and so registered
/// for rollback by the game like any other component it writes: a replay that un-happens the
/// trip un-happens the stamp. The interpolation reads it when it records, and draws the body
/// where it arrived from the first frame rather than sliding it there from where it left.
///
/// A tick rather than a marker, because a pass of [`TickedLoop`](crate::TickedLoop) can run
/// several ticks and a replay many: what matters is whether the jump happened since the pose
/// was last recorded, not whether it happened on the tick that is being recorded now.
#[derive(
    Component, Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize,
)]
pub struct Teleported(pub Tick);

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
            record_tick_states.in_set(crate::TickedSystems::PostTick),
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
///
/// A body [`Teleported`] since the last record gets the new state on both sides of the pair,
/// so the blend holds it where it arrived.
fn record_tick_states(
    tick: Res<CurrentTick>,
    mut bodies: Query<(&Transform, &mut TickedInterpolation, Option<&Teleported>)>,
) {
    for (transform, mut interpolation, teleported) in &mut bodies {
        // A clock that went backwards — a rewind, a new session on a kept entity — makes the
        // last record meaningless as a reference: anything stamped since counts.
        let since = interpolation
            .recorded_at
            .filter(|&recorded| recorded <= tick.0);
        let jumped =
            teleported.is_some_and(|stamp| since.is_none_or(|recorded| stamp.0 > recorded));
        interpolation.previous = if jumped {
            Some(*transform)
        } else {
            interpolation.current
        };
        interpolation.current = Some(*transform);
        interpolation.recorded_at = Some(tick.0);
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
            recorded_at: None,
        };
        assert_eq!(state.sample(0.0).unwrap().translation.x, 0.0);
        assert_eq!(state.sample(1.0).unwrap().translation.x, 10.0);
        assert_eq!(state.sample(0.25).unwrap().translation.x, 2.5);
    }
}
