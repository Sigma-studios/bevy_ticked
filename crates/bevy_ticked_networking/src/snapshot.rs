use std::collections::HashMap;

use bevy::prelude::*;
use serde::{Deserialize, Serialize};

use bevy_ticked::{registry::TickedComponentRegistry, tick::CurrentTick};

/// A serializable snapshot of the entire tracked world state at a specific tick.
///
/// Contains all registered component types that support serialization,
/// keyed by their `u16` type index, with each entity's component data
/// serialized as bytes keyed by `TickTrackedEntity`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorldSnapshot {
    pub tick: u64,
    /// component_type_index -> (tracked_entity_id -> serialized_component_bytes)
    pub components: HashMap<u16, HashMap<u64, Vec<u8>>>,
}

/// Build a snapshot of the current world state at the given tick.
pub fn build_snapshot(world: &mut World, tick: u64) -> WorldSnapshot {
    let registry = world.resource::<TickedComponentRegistry>().clone();
    let components = registry.serialize_all(world, tick);
    WorldSnapshot { tick, components }
}

/// Apply a network snapshot: store its data in WorldActions and restore to that tick.
pub fn apply_snapshot(world: &mut World, snapshot: &WorldSnapshot) {
    let registry = world.resource::<TickedComponentRegistry>().clone();
    registry.deserialize_and_apply_all(world, snapshot.tick, &snapshot.components);
    world.resource_mut::<CurrentTick>().0 = snapshot.tick;
}
