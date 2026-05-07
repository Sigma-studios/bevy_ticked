use std::collections::HashMap;

use bevy::prelude::*;

use crate::registry::TickedComponent;

/// Stores the history of a registered component type across all ticks and entities.
///
/// One `WorldActions<T>` resource exists per registered component type.
/// Maps `tick -> (entity_network_id -> component_value)`.
#[derive(Resource)]
pub struct WorldActions<T: TickedComponent> {
    pub(crate) history: HashMap<u64, HashMap<u64, T>>,
}

impl<T: TickedComponent> Default for WorldActions<T> {
    fn default() -> Self {
        Self {
            history: HashMap::new(),
        }
    }
}

impl<T: TickedComponent> WorldActions<T> {
    /// Get the state of all entities at a given tick.
    pub fn at_tick(&self, tick: u64) -> Option<&HashMap<u64, T>> {
        self.history.get(&tick)
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
        self.history.retain(|&t, _| t <= tick);
    }
}
