use bevy::prelude::*;
use serde::{Deserialize, Serialize};

/// Marks an entity as tracked by the tick system. The `u64` value identifies
/// this entity in the tick history (WorldActions).
///
/// This is a local concept — it has no network semantics. The networking crate
/// adds `NetworkEntityId` on top and manages the mapping.
#[derive(Component, Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TickTrackedEntity(pub u64);

/// Counter for assigning unique `TickTrackedEntity` IDs.
#[derive(Resource, Default)]
pub struct TickTrackedEntityCounter(pub u64);

impl TickTrackedEntityCounter {
    pub fn next(&mut self) -> TickTrackedEntity {
        self.0 += 1;
        TickTrackedEntity(self.0)
    }
}
