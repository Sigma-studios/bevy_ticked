use std::collections::{BTreeMap, HashMap};

use bevy::prelude::*;

use crate::registry::TickedComponent;

/// Stores the history of a registered component type across all ticks and entities.
///
/// One `WorldActions<T>` resource exists per registered component type.
/// Maps `tick -> (entity_network_id -> component_value)`.
///
/// Uses a `BTreeMap` for the outer tick index so that truncation and pruning
/// can be done in O(log n) via `split_off` rather than O(n) `retain`.
#[derive(Resource)]
pub struct WorldActions<T: TickedComponent> {
    pub(crate) history: BTreeMap<u64, HashMap<u64, T>>,
}

impl<T: TickedComponent> Default for WorldActions<T> {
    fn default() -> Self {
        Self {
            history: BTreeMap::new(),
        }
    }
}

impl<T: TickedComponent> WorldActions<T> {
    /// Get the state of all entities at a given tick.
    pub fn at_tick(&self, tick: u64) -> Option<&HashMap<u64, T>> {
        self.history.get(&tick)
    }

    /// The oldest tick still retained in history, if any.
    pub fn oldest_recorded_tick(&self) -> Option<u64> {
        self.history.keys().next().copied()
    }

    /// The newest tick recorded in history, if any.
    pub fn newest_recorded_tick(&self) -> Option<u64> {
        self.history.keys().next_back().copied()
    }

    /// The inclusive `(oldest, newest)` range of recorded ticks, if any exist.
    ///
    /// Useful for sizing a scrub bar precisely instead of guessing from
    /// `CurrentTick - HISTORY_BUFFER_TICKS`.
    pub fn recorded_range(&self) -> Option<(u64, u64)> {
        Some((self.oldest_recorded_tick()?, self.newest_recorded_tick()?))
    }

    /// Iterate over all recorded ticks in ascending order.
    pub fn recorded_ticks(&self) -> impl DoubleEndedIterator<Item = u64> + '_ {
        self.history.keys().copied()
    }

    /// Insert state for a specific entity at a specific tick.
    pub fn insert(&mut self, tick: u64, entity_network_id: u64, component: T) {
        self.history
            .entry(tick)
            .or_default()
            .insert(entity_network_id, component);
    }

    /// Replace all state at a given tick.
    pub fn set_tick(&mut self, tick: u64, state: HashMap<u64, T>) {
        self.history.insert(tick, state);
    }

    /// Remove all history after a given tick (exclusive).
    /// Used after rollback to discard invalidated future state.
    pub fn truncate_after(&mut self, tick: u64) {
        self.history.split_off(&(tick + 1));
    }

    /// Remove all history before a given tick.
    /// Used to bound memory growth during long sessions.
    pub fn prune_before(&mut self, tick: u64) {
        let kept = self.history.split_off(&tick);
        self.history = kept;
    }

    /// Clear all history.
    pub fn clear(&mut self) {
        self.history.clear();
    }
}
