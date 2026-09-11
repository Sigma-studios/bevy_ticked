//! Finding a tracked entity by the id it is known by everywhere else.
//!
//! An `Entity` is a local index. The same body is a different `Entity` on every peer, and the
//! `u64` in [`TickTrackedEntity`] is the only name for it that two machines agree on — so
//! anything that has to refer to an entity across a wire refers to it by that id and needs a way
//! back. Every consumer of this crate has written the same `HashMap<u64, Entity>` and the same
//! pair of observers to maintain it, and `apply_snapshot` walks every tracked entity to rebuild
//! the equivalent on every snapshot it applies, sixty-four times a second, then throws it away.
//!
//! This is that map, maintained once.
//!
//! # The reassignment guard is the part worth reading
//!
//! [`unindex_tracked`] checks that the entry it is about to remove still points at the entity
//! being despawned. Ids are reissued — `apply_snapshot` spawns a fresh entity for an id it has
//! not seen, and a session reset can hand out an id that was in use a moment ago — so the `Add`
//! for the new entity can land before the `Remove` for the old one. Removing unconditionally
//! would then unindex the live entity and leave the map claiming the id does not exist, which
//! reads as "the host sent me a rocket that belongs to nobody".

use bevy::platform::collections::HashMap;
use bevy::prelude::*;

use crate::tracked_entity::TickTrackedEntity;

/// `TickTrackedEntity` id → the local [`Entity`] carrying it.
///
/// Maintained by observers, so it is correct the moment the entity exists rather than at the next
/// run of some system. Inserted by [`TickedPlugin`](crate::TickedPlugin).
#[derive(Resource, Default, Debug)]
pub struct TrackedEntityIndex {
    by_id: HashMap<u64, Entity>,
    /// Ids that are tombstoned: kept, disabled, not in `by_id`.
    tombstones: HashMap<u64, Entity>,
}

impl TrackedEntityIndex {
    /// The entity carrying `id`, if it is alive in this world. A tombstone is not.
    pub fn get(&self, id: u64) -> Option<Entity> {
        self.by_id.get(&id).copied()
    }

    /// The tombstoned entity for `id`, if there is one.
    pub fn tombstone_of(&self, id: u64) -> Option<Entity> {
        self.tombstones.get(&id).copied()
    }

    /// Every `(id, entity)` tombstoned, in no particular order.
    pub fn tombstones(&self) -> impl Iterator<Item = (Entity, u64)> + '_ {
        self.tombstones.iter().map(|(id, entity)| (*entity, *id))
    }

    pub(crate) fn tombstone(&mut self, id: u64, entity: Entity) {
        if self.by_id.get(&id) == Some(&entity) {
            self.by_id.remove(&id);
        }
        self.tombstones.insert(id, entity);
    }

    pub(crate) fn revive(&mut self, id: u64, entity: Entity) {
        if self.tombstones.get(&id) == Some(&entity) {
            self.tombstones.remove(&id);
        }
        self.by_id.insert(id, entity);
    }

    pub fn contains(&self, id: u64) -> bool {
        self.by_id.contains_key(&id)
    }

    pub fn len(&self) -> usize {
        self.by_id.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_id.is_empty()
    }

    /// Every `(id, entity)` currently tracked, in no particular order.
    pub fn iter(&self) -> impl Iterator<Item = (u64, Entity)> + '_ {
        self.by_id.iter().map(|(id, entity)| (*id, *entity))
    }

    /// Every id currently tracked, ascending.
    ///
    /// Sorted because the callers that want this are building something order-sensitive — a
    /// snapshot's entity list, a checksum — and hash order is not stable between runs.
    pub fn ids(&self) -> Vec<u64> {
        let mut ids: Vec<u64> = self.by_id.keys().copied().collect();
        ids.sort_unstable();
        ids
    }
}

pub(crate) fn index_tracked(
    add: On<Add, TickTrackedEntity>,
    tracked: Query<&TickTrackedEntity>,
    mut index: ResMut<TrackedEntityIndex>,
) {
    if let Ok(id) = tracked.get(add.entity) {
        index.by_id.insert(id.0, add.entity);
    }
}

pub(crate) fn unindex_tracked(
    remove: On<Remove, TickTrackedEntity>,
    tracked: Query<&TickTrackedEntity>,
    mut index: ResMut<TrackedEntityIndex>,
) {
    let Ok(id) = tracked.get(remove.entity) else {
        return;
    };
    // See the module note: an id can already have been claimed by a different entity.
    if index.by_id.get(&id.0) == Some(&remove.entity) {
        index.by_id.remove(&id.0);
    }
    if index.tombstones.get(&id.0) == Some(&remove.entity) {
        index.tombstones.remove(&id.0);
    }
}
