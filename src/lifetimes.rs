//! Which tracked entities existed when.
//!
//! # Why this is not a registered component
//!
//! Rollback restores *state*, and for a long time it could not restore *existence*: `restore_all`
//! walks the entities that currently carry [`TickTrackedEntity`] and, per registered type, either
//! inserts the saved value or removes the component. An entity spawned after the tick being
//! restored to appears in no saved map, so every registered component is stripped from it and
//! nothing despawns it — a **husk**: still tracked, still drawn, carrying none of its state, and
//! captured into every future tick for the rest of the session.
//!
//! The obvious repair — "despawn anything with no saved state" — is wrong, and the test
//! `an_entity_with_no_registered_components_is_not_mistaken_for_one_that_did_not_exist` is why. An
//! entity may legitimately carry none of the registered types at a tick. A player who has not
//! moved, an entity whose only replicated component is inserted a tick later: both have no saved
//! state and both existed. "No state" and "no entity" are the same observation, so no amount of
//! looking at component history can tell them apart.
//!
//! So existence is recorded **separately from any component**, once per captured tick, as the set
//! of ids that were alive. That is the one fact the component histories cannot express, it costs a
//! `u64` per tracked entity per tick, and it makes the question exact rather than heuristic.
//!
//! # Why it is not in the component registry either
//!
//! Registering [`TickTrackedEntity`] as an ordinary rollback component gets *part* of the way —
//! the husk at least stops being tracked — and consumers have done it. But the registry's order is
//! a wire format: indices are assigned by position and travel in every snapshot, so adding an
//! implicit entry at index 0 would shift every consumer's registrations by one and change
//! `wire_hash()` for everybody. Entity lifetime is infrastructure, not a component somebody chose
//! to replicate, and it is kept out of that index space accordingly.

use std::collections::{BTreeMap, HashSet};

use bevy::prelude::*;

use crate::tracked_entity::TickTrackedEntity;

/// The set of tracked entity ids alive at each captured tick.
///
/// Written by [`TickedComponentRegistry::capture_all`], read by
/// [`TickedComponentRegistry::restore_all`], and truncated, pruned and cleared alongside the
/// component histories so that it can never describe a tick they no longer cover.
///
/// [`TickedComponentRegistry::capture_all`]: crate::registry::TickedComponentRegistry::capture_all
/// [`TickedComponentRegistry::restore_all`]: crate::registry::TickedComponentRegistry::restore_all
#[derive(Resource, Default)]
pub struct TrackedEntityLifetimes {
    history: BTreeMap<u64, HashSet<u64>>,
}

impl TrackedEntityLifetimes {
    /// The ids alive at `tick`, or `None` if that tick was never captured or has been pruned.
    ///
    /// The distinction matters at every call site: **absent is not empty.** An empty set means
    /// "the world was captured and held no tracked entities"; `None` means "nothing is known about
    /// this tick", and acting on it as though it were empty would despawn the world.
    pub fn at_tick(&self, tick: u64) -> Option<&HashSet<u64>> {
        self.history.get(&tick)
    }

    /// Whether `id` existed at `tick`, or `None` if the tick is not covered.
    pub fn existed(&self, tick: u64, id: u64) -> Option<bool> {
        self.at_tick(tick).map(|alive| alive.contains(&id))
    }

    /// The oldest tick still retained, if any.
    pub fn oldest_recorded_tick(&self) -> Option<u64> {
        self.history.keys().next().copied()
    }

    /// The newest tick recorded, if any.
    pub fn newest_recorded_tick(&self) -> Option<u64> {
        self.history.keys().next_back().copied()
    }

    pub(crate) fn capture(&mut self, tick: u64, alive: HashSet<u64>) {
        self.history.insert(tick, alive);
    }

    pub(crate) fn truncate_after(&mut self, tick: u64) {
        self.history.split_off(&(tick + 1));
    }

    pub(crate) fn prune_before(&mut self, tick: u64) {
        let kept = self.history.split_off(&tick);
        self.history = kept;
    }

    pub(crate) fn clear(&mut self) {
        self.history.clear();
    }
}

/// Record which tracked entities are alive at `tick`.
pub(crate) fn capture_lifetimes(world: &mut World, tick: u64) {
    let alive: HashSet<u64> = {
        let mut tracked = world.query::<&TickTrackedEntity>();
        tracked.iter(world).map(|tracked| tracked.0).collect()
    };
    world
        .get_resource_or_insert_with(TrackedEntityLifetimes::default)
        .capture(tick, alive);
}

/// Despawn every tracked entity that did not exist at `tick`.
///
/// Returns the ids despawned, for the caller to log or assert on.
///
/// Does nothing at all when the tick is not covered by the lifetime history. That is the safe
/// direction and it is deliberate: restoring a tick nobody captured is already a no-op for every
/// component type, and treating an absent record as an empty one would despawn every tracked
/// entity in the world.
pub(crate) fn despawn_entities_that_did_not_exist(world: &mut World, tick: u64) -> Vec<u64> {
    let Some(alive) = world
        .get_resource::<TrackedEntityLifetimes>()
        .and_then(|lifetimes| lifetimes.at_tick(tick))
        .cloned()
    else {
        return Vec::new();
    };

    let doomed: Vec<(Entity, u64)> = {
        let mut tracked = world.query::<(Entity, &TickTrackedEntity)>();
        tracked
            .iter(world)
            .filter(|(_, tracked)| !alive.contains(&tracked.0))
            .map(|(entity, tracked)| (entity, tracked.0))
            .collect()
    };

    for (entity, _) in &doomed {
        world.despawn(*entity);
    }
    doomed.into_iter().map(|(_, id)| id).collect()
}
