use std::collections::{HashMap, HashSet};

use bevy::prelude::*;
use serde::{Deserialize, Serialize};

use bevy_ticked::{
    registry::TickedComponentRegistry,
    tick::CurrentTick,
    tracked_entity::{TickTrackedEntity, TickTrackedEntityCounter},
};

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

/// Apply a network snapshot: sync entity lifecycle, apply component state, update tick.
///
/// This implements snapshot-implies-existence:
/// - Entities in the snapshot but not local are **spawned**
/// - Local networked entities not in the snapshot are **despawned**
/// - Existing entities get their components updated
///
/// Newly spawned entities get all networked components inserted first, then
/// `TickTrackedEntity` is inserted last. This means `On<Add, TickTrackedEntity>`
/// observers can read the networked components via Query.
pub fn apply_snapshot(world: &mut World, snapshot: &WorldSnapshot) {
    let registry = world.resource::<TickedComponentRegistry>().clone();

    // 1. Collect the full set of entity IDs present in the snapshot
    let snapshot_entity_ids: HashSet<u64> = snapshot
        .components
        .values()
        .flat_map(|entities| entities.keys().copied())
        .collect();

    // 2. Query all existing TickTrackedEntity entities
    let mut query = world.query::<(Entity, &TickTrackedEntity)>();
    let existing: Vec<(Entity, u64)> = query
        .iter(world)
        .map(|(e, tte)| (e, tte.0))
        .collect();

    let existing_ids: HashSet<u64> = existing.iter().map(|(_, id)| *id).collect();

    // 3. Despawn local entities NOT in the snapshot
    for (entity, id) in &existing {
        if !snapshot_entity_ids.contains(id) {
            world.despawn(*entity);
        }
    }

    // 4. Spawn entities in the snapshot but NOT local
    for &new_id in &snapshot_entity_ids {
        if existing_ids.contains(&new_id) {
            continue;
        }

        let entity = world.spawn_empty().id();

        // Insert all networked components from the snapshot for this entity
        for (type_index, entities) in &snapshot.components {
            if let Some(bytes) = entities.get(&new_id) {
                registry.deserialize_and_insert_one(world, *type_index, entity, bytes);
            }
        }

        // Insert TickTrackedEntity LAST so On<Add> observers can read components
        world.entity_mut(entity).insert(TickTrackedEntity(new_id));
    }

    // 5. Apply snapshot to existing (surviving) entities + write into WorldActions
    registry.deserialize_and_apply_all(world, snapshot.tick, &snapshot.components);

    // 6. Reset counter to max snapshot ID so that rollback+replay produces
    //    deterministic entity IDs matching the server.
    if let Some(&max_id) = snapshot_entity_ids.iter().max() {
        let mut counter = world.resource_mut::<TickTrackedEntityCounter>();
        counter.0 = max_id;
    }

    // 7. Set current tick
    world.resource_mut::<CurrentTick>().0 = snapshot.tick;
}
