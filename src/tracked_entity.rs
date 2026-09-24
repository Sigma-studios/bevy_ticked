//! The id every peer agrees on, and who may mint one.
//!
//! # Slots
//!
//! An id is `sequence << 24 | stream << 8 | slot`. The slot says which peer minted it: `0` is the
//! authority, `1..=255` are handed to clients at the join. Two peers minting in the same tick can
//! never collide, which is what lets a client predict a spawn — a bullet leaving its own gun — and
//! have the host confirm it under the *same id* rather than a different one it must be reconciled
//! with. Before slots, a client could not mint at all: every id it invented would be reissued by
//! the host to something else, and `apply_snapshot` would merge the two.
//!
//! # Streams
//!
//! Inside a slot, every *kind of thing* counts on its own. The stream is a hash of the wire names
//! of the networked components a spawn's bundle carries — see [`stream_of`] — so a pellet, a blast
//! and a gun minted under one slot draw from three counters.
//!
//! One counter per slot was the whole of it before, and it made the ids after any disagreement
//! name different things on different peers. A client does disagree with the authority, all the
//! time and legitimately: it guesses remote inputs, and it does not run what only the authority
//! may (refilling a weapon pad). Each spawn one side made and the other did not shifted every later
//! id in that slot by one, so the authority's gun arrived under the id the client had given its
//! predicted grenade blast — an id the client still held *alive*, so the record was decoded onto
//! the blast and nothing ever told the game to dress it again. A gun drawn as a blast, forever.
//!
//! Keyed by shape, a disagreement can only shift ids between spawns of the same kind of thing,
//! and those are dressed alike: the snapshot corrects the state and nothing on screen is wrong for
//! longer than a correction takes. It is also exactly the line [`SpawnedAs`] already draws for a
//! replay — the same bundle is the same thing coming back.
//!
//! Wire names rather than `TypeId`s because peers are different builds (a native client and a
//! web one), and wire names are what those builds already agree on. Sixteen bits of stream: two
//! kinds that hash alike share a counter, which is precisely how every kind behaved before, never
//! worse.
//!
//! # The allocator is rolled back
//!
//! [`TrackedIdAllocator`] is a ticked resource. A rewind puts its counters back, so a replay
//! that re-runs the same spawn mints the same id and the tombstone the rewind left is revived
//! with the same `Entity`. Registering it on the wire (the networking crate does) lets the
//! authority's snapshot correct a client's counters.

use std::any::TypeId;

use bevy::ecs::component::Components;
use bevy::ecs::system::SystemParam;
use bevy::prelude::*;
use serde::{Deserialize, Serialize};

use crate::lifetimes::{redress, reset, revive};
use crate::registry::{FNV_OFFSET, TickedComponentRegistry, fnv_fold};
use crate::tracked_index::TrackedEntityIndex;

/// How many low bits of an id name the spawner slot.
pub const SLOT_BITS: u32 = 8;

/// How many bits above the slot name the stream. See the module docs.
pub const STREAM_BITS: u32 = 16;

/// Marks an entity as tracked by the tick system. The `u64` value identifies this entity in
/// the tick history and across the wire. See the module docs for its shape.
#[derive(Component, Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TickTrackedEntity(pub u64);

impl TickTrackedEntity {
    /// The `sequence`th id minted by `slot` outside any stream: what [`TrackedIdAllocator::next`]
    /// hands out.
    pub fn new(slot: SpawnerSlot, sequence: u64) -> Self {
        Self::new_in(slot, 0, sequence)
    }

    /// The `sequence`th id minted by `slot` in `stream`.
    pub fn new_in(slot: SpawnerSlot, stream: u16, sequence: u64) -> Self {
        Self(
            (sequence << (SLOT_BITS + STREAM_BITS))
                | (u64::from(stream) << SLOT_BITS)
                | u64::from(slot.0),
        )
    }

    pub fn slot(&self) -> SpawnerSlot {
        SpawnerSlot((self.0 & ((1 << SLOT_BITS) - 1)) as u8)
    }

    /// The kind of thing this id was minted for, as a hash; `0` for an id minted outside any.
    pub fn stream(&self) -> u16 {
        ((self.0 >> SLOT_BITS) & ((1 << STREAM_BITS) - 1)) as u16
    }

    pub fn sequence(&self) -> u64 {
        self.0 >> (SLOT_BITS + STREAM_BITS)
    }
}

/// The stream a spawn of bundle `B` mints in: a hash of the wire names of its networked
/// components, sorted, so the order a bundle is written in does not matter and neither does the
/// order anything was registered in.
///
/// `0`, the stream outside any, when there is no registry or nothing in the bundle is networked:
/// a peer that never talks to another has nothing to agree with. Never `0` otherwise.
///
/// A component the ECS has never heard of cannot be named here, which is why registering a
/// ticked type also registers it with the world. Types that are neither are not on the wire and
/// were never going to be counted.
pub fn stream_of<B: Bundle>(
    components: &Components,
    registry: Option<&TickedComponentRegistry>,
) -> u16 {
    let Some(registry) = registry else { return 0 };
    let mut names: Vec<&'static str> = B::get_component_ids(components)
        .flatten()
        .filter_map(|id| components.get_info(id)?.type_id())
        .filter_map(|type_id| registry.networked_wire_name(type_id))
        .collect();
    if names.is_empty() {
        return 0;
    }
    names.sort_unstable();
    names.dedup();
    let hash = names.iter().fold(FNV_OFFSET, |hash, name| {
        fnv_fold(fnv_fold(hash, name.as_bytes()), b"\0")
    });
    let folded = (hash ^ (hash >> 16) ^ (hash >> 32) ^ (hash >> 48)) as u16;
    // `0` is the stream outside any; a shape that hashes there shares stream 1 instead, which
    // costs what any other pair of shapes that hash alike costs.
    folded.max(1)
}

/// What a tracked id was last minted as: the type of the bundle it was spawned with.
///
/// The discriminator between the two things a revived tombstone can be. A replay re-running the
/// spawn it ran before passes the very same bundle type; an id that has been handed to something
/// else — the corrected timeline spawning a piece of a ragdoll where the mispredicted one spawned
/// a pellet — passes a different one. Only the second is a reason to dress the entity again.
///
/// Since ids are minted per stream the second case needs two bundle types with the same
/// networked shape, but it is still the local path's exact answer and costs nothing.
///
/// The obvious alternative, comparing the entity's shape before and after the bundle goes on,
/// does not work and is worth saying why: a tombstone's components are mutated by things that
/// have nothing to do with who owns the id. A predicted spawn the authority has not seen loses
/// its networked types to `finish_wire_tick`'s absence rule while it waits, so the replayed
/// spawn puts them back and the shape "changes" on every single rollback.
///
/// Local-only and unregistered, so nothing captures, restores or strips it.
#[derive(Component, Clone, Copy, Debug, PartialEq, Eq)]
pub struct SpawnedAs(pub TypeId);

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

/// The next sequence number for every `(slot, stream)` that has minted. Ticked (rolled back)
/// and, with the networking crate, networked as `"bevy_ticked::TrackedIdAllocator"`.
///
/// Sorted by `(slot, stream)` and holding only counters that have moved, so an untouched pair is
/// `1` without being stored, equality is structural, and on the wire only what has minted
/// travels. A game has a few dozen kinds of thing at most.
#[derive(Resource, Clone, Debug, Default, PartialEq, Eq)]
pub struct TrackedIdAllocator {
    next: Vec<(SpawnerSlot, u16, u64)>,
}

impl Serialize for TrackedIdAllocator {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let used: Vec<(u8, u16, u64)> = self
            .next
            .iter()
            .map(|(slot, stream, next)| (slot.0, *stream, *next))
            .collect();
        used.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for TrackedIdAllocator {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let used = Vec::<(u8, u16, u64)>::deserialize(deserializer)?;
        let mut allocator = Self::default();
        for (slot, stream, next) in used {
            allocator.set(SpawnerSlot(slot), stream, next);
        }
        Ok(allocator)
    }
}

impl TrackedIdAllocator {
    fn position(&self, slot: SpawnerSlot, stream: u16) -> Result<usize, usize> {
        self.next
            .binary_search_by_key(&(slot, stream), |(slot, stream, _)| (*slot, *stream))
    }

    fn set(&mut self, slot: SpawnerSlot, stream: u16, next: u64) {
        if next <= 1 {
            if let Ok(at) = self.position(slot, stream) {
                self.next.remove(at);
            }
            return;
        }
        match self.position(slot, stream) {
            Ok(at) => self.next[at].2 = next,
            Err(at) => self.next.insert(at, (slot, stream, next)),
        }
    }

    /// Mint the next id for `slot`, outside any stream.
    ///
    /// For code that spawns by hand. [`TrackedSpawner`] mints in the bundle's stream instead,
    /// which is what keeps a disagreement between peers about one kind of thing from renaming
    /// every other kind — see the module docs.
    pub fn next(&mut self, slot: SpawnerSlot) -> TickTrackedEntity {
        self.next_in(slot, 0)
    }

    /// Mint the next id for `slot` in `stream`.
    pub fn next_in(&mut self, slot: SpawnerSlot, stream: u16) -> TickTrackedEntity {
        let sequence = self.peek_in(slot, stream);
        self.set(slot, stream, sequence + 1);
        TickTrackedEntity::new_in(slot, stream, sequence)
    }

    /// Mint the next id as the authority, outside any stream.
    pub fn next_authority(&mut self) -> TickTrackedEntity {
        self.next(SpawnerSlot::AUTHORITY)
    }

    /// The sequence the next `next(slot)` would use.
    pub fn peek(&self, slot: SpawnerSlot) -> u64 {
        self.peek_in(slot, 0)
    }

    /// The sequence the next `next_in(slot, stream)` would use.
    pub fn peek_in(&self, slot: SpawnerSlot, stream: u16) -> u64 {
        self.position(slot, stream).map_or(1, |at| self.next[at].2)
    }

    /// How many ids `slot` has minted, over every stream. Moves exactly when the slot mints.
    pub fn minted_by(&self, slot: SpawnerSlot) -> u64 {
        self.next
            .iter()
            .filter(|(minter, _, _)| *minter == slot)
            .map(|(_, _, next)| next - 1)
            .sum()
    }

    /// Make sure `id` is never minted again by its slot and stream: a host taking over a world, a
    /// client applying a snapshot that names ids it has not seen.
    pub fn raise_to(&mut self, id: TickTrackedEntity) {
        let (slot, stream) = (id.slot(), id.stream());
        let next = self.peek_in(slot, stream).max(id.sequence() + 1);
        self.set(slot, stream, next);
    }

    /// Every `(slot, stream, next sequence)` that has minted, sorted. For a hash or a debug line.
    pub fn sequences(&self) -> impl ExactSizeIterator<Item = (SpawnerSlot, u16, u64)> + '_ {
        self.next.iter().copied()
    }
}

/// Spawn tracked entities with ids every peer will agree on.
///
/// `spawn` mints from this peer's [`LocalSpawnerSlot`] (the authority's if none); `spawn_by`
/// from a slot the caller names — a bullet spawned on every peer by the player who fired it,
/// under that player's slot, so every peer mints the same id. If the id has a tombstone (a
/// rewind undid this very spawn a moment ago), the tombstone is revived with the new bundle
/// on it, so the `Entity` is the one the game already held — and [`redress`] fires the game's
/// `On<Add, TickTrackedEntity>` observer again, because an id handed back is not a promise that
/// it has come back as the same thing.
#[derive(SystemParam)]
pub struct TrackedSpawner<'w, 's> {
    commands: Commands<'w, 's>,
    allocator: ResMut<'w, TrackedIdAllocator>,
    local: Option<Res<'w, LocalSpawnerSlot>>,
    index: Res<'w, TrackedEntityIndex>,
    /// To tell a tombstone that is still there from one the reaper has destroyed.
    entities: &'w bevy::ecs::entity::Entities,
    tick: Res<'w, crate::tick::CurrentTick>,
    /// To name a bundle's networked components, for its stream.
    components: &'w Components,
    registry: Option<Res<'w, TickedComponentRegistry>>,
}

impl TrackedSpawner<'_, '_> {
    /// This peer's slot.
    pub fn local_slot(&self) -> SpawnerSlot {
        self.local.as_ref().map_or(SpawnerSlot::AUTHORITY, |l| l.0)
    }

    /// Spawn `bundle` as a tracked entity minted by this peer.
    pub fn spawn<B: Bundle>(&mut self, bundle: B) -> Entity {
        let slot = self.local_slot();
        self.spawn_by(slot, bundle)
    }

    /// Spawn `bundle` as a tracked entity minted by `slot`.
    pub fn spawn_by<B: Bundle>(&mut self, slot: SpawnerSlot, bundle: B) -> Entity {
        let stream = stream_of::<B>(self.components, self.registry.as_deref());
        let id = self.allocator.next_in(slot, stream);
        let tick = self.tick.0;
        let minted_as = SpawnedAs(TypeId::of::<B>());
        // A handle out of the index is a hint, not a promise. The reaper destroys a tombstone once
        // the window has passed it, so one that is no longer in the world means this id is being
        // minted afresh — and commanding the dead handle instead is the crash this guard is for.
        let tombstone = self
            .index
            .tombstone_of(id.0)
            .filter(|entity| self.entities.contains(*entity));
        match tombstone {
            Some(entity) => {
                self.commands
                    .entity(entity)
                    .queue(move |mut entity: EntityWorldMut| {
                        // See [`SpawnedAs`]: the same bundle type is the replay re-running a
                        // spawn it has run before, and dressing that again would fire the game's
                        // observer once per rollback.
                        let changed_hands = entity.get::<SpawnedAs>().copied() != Some(minted_as);
                        let e = entity.id();
                        if changed_hands {
                            // Before the bundle goes on: a different thing starts from nothing
                            // rather than from whatever the last occupant left here.
                            entity.world_scope(|world| reset(world, e));
                        }
                        entity.insert((bundle, minted_as));
                        entity.world_scope(|world| {
                            revive(world, e, id.0, tick);
                            if changed_hands {
                                // After the bundle, so the observer sees what it is now.
                                redress(world, e, id.0);
                            }
                        });
                    });
                entity
            }
            None => self.commands.spawn((bundle, id, minted_as)).id(),
        }
    }
}

/// [`TrackedSpawner`] for code that holds a `&mut World`.
pub trait TrackedWorldExt {
    /// Spawn `bundle` as a tracked entity minted by this peer's slot.
    fn spawn_tracked<B: Bundle>(&mut self, bundle: B) -> Entity;
    /// Spawn `bundle` as a tracked entity minted by `slot`.
    fn spawn_tracked_by<B: Bundle>(&mut self, slot: SpawnerSlot, bundle: B) -> Entity;
}

impl TrackedWorldExt for World {
    fn spawn_tracked<B: Bundle>(&mut self, bundle: B) -> Entity {
        let slot = self
            .get_resource::<LocalSpawnerSlot>()
            .map_or(SpawnerSlot::AUTHORITY, |l| l.0);
        self.spawn_tracked_by(slot, bundle)
    }

    fn spawn_tracked_by<B: Bundle>(&mut self, slot: SpawnerSlot, bundle: B) -> Entity {
        let stream = stream_of::<B>(
            self.components(),
            self.get_resource::<TickedComponentRegistry>(),
        );
        let id = self
            .resource_mut::<TrackedIdAllocator>()
            .next_in(slot, stream);
        let minted_as = SpawnedAs(TypeId::of::<B>());
        let tick = self
            .get_resource::<crate::tick::CurrentTick>()
            .map_or(0, |t| t.0);
        // A handle out of the index is a hint, not a promise: the reaper destroys tombstones once
        // the window has passed them, so one that is no longer in the world means this id is being
        // minted afresh rather than revived.
        let tombstone = self
            .get_resource::<TrackedEntityIndex>()
            .and_then(|index| index.tombstone_of(id.0))
            .filter(|entity| self.get_entity(*entity).is_ok());
        match tombstone {
            Some(entity) => {
                // See the note in `TrackedSpawner::spawn_by`: the same bundle type is the same
                // spawn coming round again, and must not be dressed a second time.
                let changed_hands = self.get::<SpawnedAs>(entity).copied() != Some(minted_as);
                if changed_hands {
                    // Before the bundle, for the reason in `TrackedSpawner::spawn_by`.
                    reset(self, entity);
                }
                self.entity_mut(entity).insert((bundle, minted_as));
                revive(self, entity, id.0, tick);
                if changed_hands {
                    redress(self, entity, id.0);
                }
                entity
            }
            // `SpawnedAs` on a fresh spawn too, as `TrackedSpawner` does: without it the first
            // replay to revive this id's tombstone reads "changed hands" and dresses it again.
            None => self.spawn((bundle, id, minted_as)).id(),
        }
    }
}
