//! The snapshot: what the authority says the world is, and how it travels.
//!
//! # Shape
//!
//! Entity-major. One [`EntityRecord`] per tracked entity, its networked components concatenated
//! in wire-index order with no length prefixes, because postcard is self-delimiting; a
//! [`TypeMask`] says which types are there. Resources follow as `(wire index, bytes)` pairs.
//! Everything is sorted, so the same world encodes to the same bytes twice, which is what a
//! delta (the [`SnapshotBody::Delta`] variant, reserved here and filled in by the delta phase)
//! needs to be built against.
//!
//! The previous shape was type-major (`type -> id -> bytes`) in `HashMap`s: every entity's id
//! travelled once per type it carried, every component carried a length prefix, and the maps
//! encoded in a different order each time. Two walking players cost 1340 bytes a tick.
//!
//! # Per recipient
//!
//! A packet is addressed. `seq` counts the packets sent to *that* client, and `your_margin` is
//! that client's own input-arrival margin — the previous shape carried every client's margin to
//! every client, which was a byte per player per tick of nothing and a small leak of who is
//! lagging.

use std::collections::HashSet;

use bevy::prelude::*;
use serde::{Deserialize, Serialize};

use bevy_ticked::{
    lifetimes::{TrackedEntityLifetimes, revive, tombstone_at},
    registry::{TickedComponentRegistry, TypeMask},
    resource_registry::TickedResourceRegistry,
    tick::CurrentTick,
    tracked_entity::{TickTrackedEntity, TrackedIdAllocator},
    tracked_index::TrackedEntityIndex,
};

/// One snapshot, as sent to one client.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotPacket {
    /// Counts the packets sent to this recipient. A client acks it on its input packets.
    pub seq: u32,
    /// The tick the body describes.
    pub tick: u64,
    /// This recipient's input-arrival margin in ticks, measured by the server: how many ticks
    /// ahead of the server its most recent input arrived (negative is late). The client sizes
    /// its prediction lead from it.
    pub your_margin: i16,
    pub body: SnapshotBody,
}

/// What a packet carries.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SnapshotBody {
    /// The whole tracked world at `tick`.
    Full(FullBody),
    /// What changed since a packet the recipient acknowledged. Reserved: built and applied by
    /// the delta phase. Until then a client drops it and counts the drop.
    Delta(DeltaBody),
}

/// Every tracked entity and every networked resource, at one tick.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FullBody {
    /// Sorted by id.
    pub entities: Vec<EntityRecord>,
    /// `(wire index, bytes)`, sorted by wire index. A resource that is absent here is one the
    /// authority said nothing about, **not** one it removed: a resource is one value with no
    /// entity to be missing from.
    pub resources: Vec<(u16, Vec<u8>)>,
    /// Other players' inputs the server already holds for ticks after `tick`, so a client's
    /// replay can use what those players actually pressed rather than nothing.
    pub inputs_ahead: Vec<RelayedInput>,
}

/// The change since a baseline. Reserved for the delta phase.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeltaBody {
    /// `seq` of the packet this delta is against.
    pub baseline_seq: u32,
    pub changed: Vec<EntityRecord>,
    /// Components removed from entities that still exist.
    pub removed: Vec<(u64, TypeMask)>,
    pub despawned: Vec<u64>,
    pub resources: Vec<(u16, Vec<u8>)>,
    pub inputs_ahead: Vec<RelayedInput>,
}

/// One tracked entity's networked state.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EntityRecord {
    pub id: u64,
    /// Which wire types `bytes` holds.
    pub present: TypeMask,
    /// The present types' values, concatenated in wire-index order, no prefixes.
    pub bytes: Vec<u8>,
}

impl EntityRecord {
    /// An empty record for `id`; add components with [`with`](Self::with) in ascending
    /// wire-index order.
    pub fn new(id: u64) -> Self {
        Self {
            id,
            ..Default::default()
        }
    }

    /// Append `value` as wire type `wire_index`. Indices must be added in ascending order,
    /// which is the order a decoder reads them in.
    ///
    /// # Panics
    ///
    /// If `wire_index` is not above every index already present.
    pub fn with<T: Serialize>(mut self, wire_index: u16, value: &T) -> Self {
        if let Some(last) = self.present.iter().last() {
            assert!(
                wire_index > last,
                "components go into a record in ascending wire-index order ({wire_index} after \
                 {last})"
            );
        }
        self.present.set(wire_index);
        self.bytes = postcard::to_extend(value, std::mem::take(&mut self.bytes))
            .expect("a networked component always encodes");
        self
    }

    /// Decode the *first* present type of this record as `T`, without a world.
    ///
    /// Postcard cannot skip a value whose type it does not know, so a record can only be read
    /// from the front without the registry; a test that wants one type puts it first, or asks
    /// the world after [`apply_full_body`].
    pub fn first<T: serde::de::DeserializeOwned>(&self) -> Option<T> {
        postcard::take_from_bytes::<T>(&self.bytes)
            .ok()
            .map(|(value, _)| value)
    }
}

impl FullBody {
    /// The record for `id`, if the body has one.
    pub fn record(&self, id: u64) -> Option<&EntityRecord> {
        self.entities.iter().find(|record| record.id == id)
    }

    /// Every id in the body, ascending.
    pub fn ids(&self) -> impl Iterator<Item = u64> + '_ {
        self.entities.iter().map(|record| record.id)
    }

    /// Replace or add the record for `record.id`, keeping the body sorted.
    pub fn put(&mut self, record: EntityRecord) {
        match self.entities.binary_search_by_key(&record.id, |r| r.id) {
            Ok(at) => self.entities[at] = record,
            Err(at) => self.entities.insert(at, record),
        }
    }

    /// Drop the record for `id`.
    pub fn remove(&mut self, id: u64) {
        self.entities.retain(|record| record.id != id);
    }
}

impl SnapshotPacket {
    /// A full packet for `tick` with no sequence or margin, as a test or a tool builds one.
    pub fn full(tick: u64, body: FullBody) -> Self {
        Self {
            seq: 0,
            tick,
            your_margin: 0,
            body: SnapshotBody::Full(body),
        }
    }

    /// The full body, if this packet carries one.
    pub fn full_body(&self) -> Option<&FullBody> {
        match &self.body {
            SnapshotBody::Full(body) => Some(body),
            SnapshotBody::Delta(_) => None,
        }
    }
}

/// One player's input for one tick, as the server holds it, postcard-encoded.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelayedInput {
    pub player: u128,
    pub tick: u64,
    pub bytes: Vec<u8>,
}

/// Encode the tracked world at `tick` from history.
///
/// Walks the tracked ids once, in order, and asks the registry for each id's types in wire
/// order. `inputs_ahead` is left empty; the server fills it in.
pub fn build_full_body(world: &mut World, tick: u64) -> FullBody {
    let registry = world.resource::<TickedComponentRegistry>().clone();
    let mut ids: Vec<u64> = {
        let mut tracked = world.query::<&TickTrackedEntity>();
        tracked.iter(world).map(|tracked| tracked.0).collect()
    };
    ids.sort_unstable();
    ids.dedup();

    let mut entities = Vec::with_capacity(ids.len());
    for id in ids {
        let present = registry.present_at(world, tick, id);
        let mut bytes = Vec::new();
        for wire_index in present.iter() {
            registry.encode_one(world, wire_index, tick, id, &mut bytes);
        }
        entities.push(EntityRecord { id, present, bytes });
    }
    let resources = world
        .get_resource::<TickedResourceRegistry>()
        .cloned()
        .map(|resources| resources.serialize_all(world, tick))
        .unwrap_or_default();
    FullBody {
        entities,
        resources,
        inputs_ahead: Vec::new(),
    }
}

/// What applying a body did, for the caller's bookkeeping.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Applied {
    pub spawned: Vec<u64>,
    pub despawned: Vec<u64>,
    /// Records the registry could not decode: `(id, wire index)`.
    pub undecodable: Vec<(u64, u16)>,
    /// Ids that appeared in more than one record.
    pub duplicate_ids: Vec<u64>,
}

/// Apply a full body: sync entity lifecycle, apply component state, set the tick.
///
/// Snapshot-implies-existence, with the lifetimes deciding what absence means:
/// - an id in the body but not local is **spawned** — or its tombstone **revived**, if this
///   peer despawned it and the authority says it is still there — its components inserted
///   first and `TickTrackedEntity` last, so an `On<Add, TickTrackedEntity>` observer sees them;
/// - a local tracked id not in the body is **tombstoned** if it was born at or before `tick`
///   (the authority has seen it and does not have it); one born after `tick` is left to the
///   rollback, which knows whether the replay spawns it again. That is what lets a client
///   predict a spawn without the next snapshot deleting it;
/// - an entity in both gets the body's components, and loses any networked type the body does
///   not give it (absence is authoritative).
///
/// Walks the records once. The history entry at `tick` is opened for every networked type
/// before the walk and closed after it, which is where absence is enforced, one query per type.
pub fn apply_full_body(world: &mut World, tick: u64, body: &FullBody) -> Applied {
    let registry = world.resource::<TickedComponentRegistry>().clone();
    let mut applied = Applied::default();

    // What is already here. Queried rather than read from `TrackedEntityIndex`: this takes a
    // bare `&mut World` and is called against worlds that have the registry and nothing else.
    let mut query = world.query::<(Entity, &TickTrackedEntity)>();
    let existing: Vec<(Entity, u64)> = query.iter(world).map(|(e, t)| (e, t.0)).collect();

    let body_ids: HashSet<u64> = body.entities.iter().map(|record| record.id).collect();
    let mut seen = HashSet::with_capacity(body.entities.len());

    // What the authority does not have, and has had the chance to see: gone. Immediate, so the
    // walk below never sees them. A tombstone, not a destruction, so a later word can undo it.
    let lifetimes = world.get_resource::<TrackedEntityLifetimes>().cloned();
    for (entity, id) in &existing {
        if body_ids.contains(id) {
            continue;
        }
        let born_after = lifetimes
            .as_ref()
            .and_then(|l| l.born_at(*id))
            .is_some_and(|born| born > tick);
        if born_after {
            continue;
        }
        if world.contains_resource::<TrackedEntityIndex>() {
            tombstone_at(world, *entity, *id, tick);
        } else {
            world.despawn(*entity);
        }
        applied.despawned.push(*id);
    }
    let mut by_id: std::collections::HashMap<u64, Entity> = existing
        .iter()
        .filter(|(_, id)| body_ids.contains(id))
        .map(|(entity, id)| (*id, *entity))
        .collect();

    registry.begin_wire_tick(world, tick);
    for record in &body.entities {
        if !seen.insert(record.id) {
            applied.duplicate_ids.push(record.id);
            continue;
        }
        let (entity, fresh) = match by_id.get(&record.id) {
            Some(entity) => (*entity, false),
            None => {
                // A tombstone this peer left — a predicted despawn the authority contradicts,
                // a rewind past a spawn the authority confirms — comes back as itself.
                let tombstoned = world
                    .get_resource::<TrackedEntityIndex>()
                    .and_then(|index| index.tombstone_of(record.id));
                let entity = match tombstoned {
                    Some(entity) => {
                        revive(world, entity, record.id, tick);
                        entity
                    }
                    None => world.spawn_empty().id(),
                };
                by_id.insert(record.id, entity);
                (entity, tombstoned.is_none())
            }
        };
        let mut rest: &[u8] = &record.bytes;
        for wire_index in record.present.iter() {
            match registry.decode_one(world, wire_index, tick, entity, record.id, rest) {
                Some(consumed) => rest = &rest[consumed..],
                None => {
                    applied.undecodable.push((record.id, wire_index));
                    // Nothing after a failed decode can be located.
                    break;
                }
            }
        }
        if fresh {
            world.entity_mut(entity).insert(TickTrackedEntity(record.id));
            applied.spawned.push(record.id);
        }
    }
    registry.finish_wire_tick(world, tick);

    // Everything the authority named existed at `tick`, whatever this peer had captured.
    if let Some(mut lifetimes) = world.get_resource_mut::<TrackedEntityLifetimes>() {
        for id in &body_ids {
            lifetimes.note_alive(tick, *id);
        }
    }

    if !body.resources.is_empty()
        && let Some(resources) = world.get_resource::<TickedResourceRegistry>().cloned()
    {
        resources.deserialize_and_apply_all(world, tick, &body.resources);
    }

    // Every id the authority named is one nobody may mint again.
    {
        let mut allocator = world.resource_mut::<TrackedIdAllocator>();
        for id in &body_ids {
            allocator.raise_to(TickTrackedEntity(*id));
        }
    }
    world.resource_mut::<CurrentTick>().0 = tick;
    applied
}

/// Encode a packet for the wire.
pub fn encode_packet(packet: &SnapshotPacket) -> Vec<u8> {
    postcard::to_allocvec(packet).expect("a snapshot packet always encodes")
}

/// Decode a packet from the wire. `None` for anything that is not one.
pub fn decode_packet(bytes: &[u8]) -> Option<SnapshotPacket> {
    postcard::from_bytes(bytes).ok()
}
