//! The id every peer agrees on, and who may mint one.
//!
//! # Slots
//!
//! An id is `sequence << 8 | slot`. The slot says which peer minted it: `0` is the authority,
//! `1..=255` are handed to clients at the join. Two peers minting in the same tick can never
//! collide, which is what lets a client predict a spawn — a bullet leaving its own gun — and
//! have the host confirm it under the *same id* rather than a different one it must be
//! reconciled with. Before slots, a client could not mint at all: every id it invented would be
//! reissued by the host to something else, and `apply_snapshot` would merge the two.
//!
//! # The allocator is rolled back
//!
//! [`TrackedIdAllocator`] is a ticked resource. A rewind puts its counters back, so a replay
//! that re-runs the same spawn mints the same id and the tombstone the rewind left is revived
//! with the same `Entity`. Registering it on the wire (the networking crate does) lets the
//! authority's snapshot correct a client's counters for slot 0.

use bevy::ecs::system::SystemParam;
use bevy::prelude::*;
use serde::{Deserialize, Serialize};

use crate::lifetimes::revive;
use crate::tracked_index::TrackedEntityIndex;

/// How many low bits of an id name the spawner slot.
pub const SLOT_BITS: u32 = 8;

/// Marks an entity as tracked by the tick system. The `u64` value identifies this entity in
/// the tick history and across the wire. See the module docs for its shape.
#[derive(Component, Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TickTrackedEntity(pub u64);

impl TickTrackedEntity {
    /// The `sequence`th id minted by `slot`.
    pub fn new(slot: SpawnerSlot, sequence: u64) -> Self {
        Self((sequence << SLOT_BITS) | u64::from(slot.0))
    }

    pub fn slot(&self) -> SpawnerSlot {
        SpawnerSlot((self.0 & ((1 << SLOT_BITS) - 1)) as u8)
    }

    pub fn sequence(&self) -> u64 {
        self.0 >> SLOT_BITS
    }
}

/// Who mints an id: the authority, or a client's seat.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct SpawnerSlot(pub u8);

impl SpawnerSlot {
    pub const AUTHORITY: Self = Self(0);
}

/// This peer's slot. Inserted by the networking crate when the role is known (`0` on a host,
/// the welcomed slot on a client); absent on a solo peer, which mints as the authority.
#[derive(Resource, Clone, Copy, Debug, PartialEq, Eq)]
pub struct LocalSpawnerSlot(pub SpawnerSlot);

/// The next sequence number for every slot. Ticked (rolled back) and, with the networking
/// crate, networked as `"bevy_ticked::TrackedIdAllocator"`.
///
/// A fixed array, so capturing it every tick is a copy and not an allocation; on the wire only
/// the slots that have minted travel.
#[derive(Resource, Clone, Copy, Debug, PartialEq, Eq)]
pub struct TrackedIdAllocator {
    next: [u64; 1 << SLOT_BITS],
}

impl Default for TrackedIdAllocator {
    fn default() -> Self {
        Self {
            next: [1; 1 << SLOT_BITS],
        }
    }
}

impl Serialize for TrackedIdAllocator {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let used: Vec<(u8, u64)> = self
            .next
            .iter()
            .enumerate()
            .filter(|(_, next)| **next != 1)
            .map(|(slot, next)| (slot as u8, *next))
            .collect();
        used.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for TrackedIdAllocator {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let used = Vec::<(u8, u64)>::deserialize(deserializer)?;
        let mut allocator = Self::default();
        for (slot, next) in used {
            allocator.next[usize::from(slot)] = next;
        }
        Ok(allocator)
    }
}

impl TrackedIdAllocator {
    /// Mint the next id for `slot`.
    pub fn next(&mut self, slot: SpawnerSlot) -> TickTrackedEntity {
        let sequence = &mut self.next[usize::from(slot.0)];
        let id = TickTrackedEntity::new(slot, *sequence);
        *sequence += 1;
        id
    }

    /// Mint the next id as the authority.
    pub fn next_authority(&mut self) -> TickTrackedEntity {
        self.next(SpawnerSlot::AUTHORITY)
    }

    /// The sequence the next `next(slot)` would use.
    pub fn peek(&self, slot: SpawnerSlot) -> u64 {
        self.next[usize::from(slot.0)]
    }

    /// Make sure `id` is never minted again by its slot: a host taking over a world, a client
    /// applying a snapshot that names ids it has not seen.
    pub fn raise_to(&mut self, id: TickTrackedEntity) {
        let slot = &mut self.next[usize::from(id.slot().0)];
        *slot = (*slot).max(id.sequence() + 1);
    }

    /// Every slot's next sequence, for a hash or a debug line.
    pub fn sequences(&self) -> &[u64] {
        &self.next
    }
}

/// Spawn tracked entities with ids every peer will agree on.
///
/// `spawn` mints from this peer's [`LocalSpawnerSlot`] (the authority's if none); `spawn_by`
/// from a slot the caller names — a bullet spawned on every peer by the player who fired it,
/// under that player's slot, so every peer mints the same id. If the id has a tombstone (a
/// rewind undid this very spawn a moment ago), the tombstone is revived with the new bundle
/// on it, so the `Entity` is the one the game already held.
#[derive(SystemParam)]
pub struct TrackedSpawner<'w, 's> {
    commands: Commands<'w, 's>,
    allocator: ResMut<'w, TrackedIdAllocator>,
    local: Option<Res<'w, LocalSpawnerSlot>>,
    index: Res<'w, TrackedEntityIndex>,
    tick: Res<'w, crate::tick::CurrentTick>,
}

impl TrackedSpawner<'_, '_> {
    /// This peer's slot.
    pub fn local_slot(&self) -> SpawnerSlot {
        self.local.as_ref().map_or(SpawnerSlot::AUTHORITY, |l| l.0)
    }

    /// Spawn `bundle` as a tracked entity minted by this peer.
    pub fn spawn(&mut self, bundle: impl Bundle) -> Entity {
        let slot = self.local_slot();
        self.spawn_by(slot, bundle)
    }

    /// Spawn `bundle` as a tracked entity minted by `slot`.
    pub fn spawn_by(&mut self, slot: SpawnerSlot, bundle: impl Bundle) -> Entity {
        let id = self.allocator.next(slot);
        let tick = self.tick.0;
        match self.index.tombstone_of(id.0) {
            Some(entity) => {
                self.commands
                    .entity(entity)
                    .queue(move |mut entity: EntityWorldMut| {
                        entity.insert(bundle);
                        let e = entity.id();
                        entity.world_scope(|world| revive(world, e, id.0, tick));
                    });
                entity
            }
            None => self.commands.spawn((bundle, id)).id(),
        }
    }
}

/// [`TrackedSpawner`] for code that holds a `&mut World`.
pub trait TrackedWorldExt {
    /// Spawn `bundle` as a tracked entity minted by this peer's slot.
    fn spawn_tracked(&mut self, bundle: impl Bundle) -> Entity;
    /// Spawn `bundle` as a tracked entity minted by `slot`.
    fn spawn_tracked_by(&mut self, slot: SpawnerSlot, bundle: impl Bundle) -> Entity;
}

impl TrackedWorldExt for World {
    fn spawn_tracked(&mut self, bundle: impl Bundle) -> Entity {
        let slot = self
            .get_resource::<LocalSpawnerSlot>()
            .map_or(SpawnerSlot::AUTHORITY, |l| l.0);
        self.spawn_tracked_by(slot, bundle)
    }

    fn spawn_tracked_by(&mut self, slot: SpawnerSlot, bundle: impl Bundle) -> Entity {
        let id = self.resource_mut::<TrackedIdAllocator>().next(slot);
        let tick = self
            .get_resource::<crate::tick::CurrentTick>()
            .map_or(0, |t| t.0);
        let tombstone = self
            .get_resource::<TrackedEntityIndex>()
            .and_then(|index| index.tombstone_of(id.0));
        match tombstone {
            Some(entity) => {
                self.entity_mut(entity).insert(bundle);
                revive(self, entity, id.0, tick);
                entity
            }
            None => self.spawn((bundle, id)).id(),
        }
    }
}
