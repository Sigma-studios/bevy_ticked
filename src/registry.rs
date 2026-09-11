use std::{
    any::{TypeId, type_name},
    collections::HashMap,
    sync::{Arc, OnceLock},
};

use bevy::prelude::*;

use crate::{
    resource_registry::TickedResourceRegistry, tracked_entity::TickTrackedEntity,
    world_actions::WorldActions,
};

/// FNV-1a starting value, shared by every registry hash so that the two index spaces are folded
/// the same way and a reader can reason about one from the other.
pub(crate) const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// The version of the snapshot wire format, folded into every registry hash so that two peers
/// with the same registrations but a different encoding still refuse each other at the join.
pub const PROTOCOL_VERSION: u16 = 2;

pub(crate) fn fnv_fold(mut hash: u64, bytes: &[u8]) -> u64 {
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

/// Trait bound for components that can be tracked by the tick system.
///
/// Only requires Clone for capture/restore. Automatically implemented.
pub trait TickedComponent: Component + Clone + Send + Sync + 'static {}

impl<T> TickedComponent for T where T: Component + Clone + Send + Sync + 'static {}

/// Runtime registry mapping component types to compact indices.
///
/// Each registered component type is assigned a sequential `u16` index.
/// Registration order must be the same on all peers.
///
/// Internals are `Arc`-wrapped so that cloning the registry (required to
/// work around `&mut World` borrow conflicts) is an O(1) reference-count
/// bump instead of a deep copy.
#[derive(Resource, Clone)]
pub struct TickedComponentRegistry {
    inner: Arc<RegistryInner>,
}

impl Default for TickedComponentRegistry {
    fn default() -> Self {
        Self {
            inner: Arc::new(RegistryInner::default()),
        }
    }
}

#[derive(Default, Clone)]
struct RegistryInner {
    entries: Vec<RegisteredTickedComponent>,
    type_indices: HashMap<TypeId, u16>,
    /// The wire order, computed the first time anything asks for it. After that registration
    /// is refused, because an index that has been handed out cannot move.
    frozen: OnceLock<Frozen>,
}

/// The wire index space: networked entries ranked by name.
#[derive(Clone, Debug, Default)]
struct Frozen {
    /// Registration index of the entry at each wire index.
    entries: Vec<u16>,
    /// Wire index by type.
    by_type: HashMap<TypeId, u16>,
    names: Vec<&'static str>,
    hash: u64,
}

#[derive(Clone)]
struct RegisteredTickedComponent {
    /// The name this type travels under: its identity on the wire and in the handshake.
    ///
    /// Required for anything networked, because the wire index is derived from it.
    /// Rollback-only types may leave it at `type_name::<T>()`, which is only ever shown in a
    /// log; `std::any::type_name`'s output is explicitly not guaranteed stable across compiler
    /// versions, so it never enters a hash.
    wire_name: &'static str,
    type_id: TypeId,
    capture: fn(&mut World, u64),
    restore: fn(&mut World, u64),
    truncate_after: fn(&mut World, u64),
    prune_before: fn(&mut World, u64),
    clear: fn(&mut World),
    has_tick: fn(&World, u64) -> bool,
    oldest_tick: fn(&World) -> Option<u64>,
    /// Which tracked-entity ids this type has saved state for at a tick.
    saved_ids: fn(&World, u64) -> Vec<u64>,
    /// The wire, populated by the networking crate. `None` for a rollback-only type.
    wire: Option<WireFns>,
}

/// How a networked type gets on and off the wire, entity by entity.
///
/// A snapshot is entity-major: one record per entity, its components concatenated in wire-index
/// order with no length prefixes, because postcard is self-delimiting. So the registry does not
/// serialise "all of type T"; it appends one value and takes one value.
#[derive(Clone, Copy)]
pub struct WireFns {
    /// Append the value saved for `(tick, id)` to `out`. `false` if there is none.
    pub encode_one: fn(&World, u64, u64, &mut Vec<u8>) -> bool,
    /// Take one value from the front of `bytes`, insert it on `entity` and into the history at
    /// `(tick, id)`. Returns how many bytes it consumed, or `None` if they did not decode.
    pub decode_one: fn(&mut World, u64, Entity, u64, &[u8]) -> Option<usize>,
    /// Start a new history entry at `tick`, empty; `decode_one` fills it.
    pub begin_tick: fn(&mut World, u64),
    /// Absence is authoritative: remove the type from every tracked entity that `decode_one`
    /// did not fill at `tick`.
    pub finish_tick: fn(&mut World, u64),
    /// Whether `(tick, id)` has a saved value.
    pub has_at: fn(&World, u64, u64) -> bool,
}

impl TickedComponentRegistry {
    pub fn register<T: TickedComponent>(&mut self) {
        self.register_inner::<T>(None, None);
    }

    /// Register for rollback with a stable wire name. See
    /// [`TickedAppExt::register_ticked_component_as`].
    pub fn register_as<T: TickedComponent>(&mut self, wire_name: &'static str) {
        self.register_inner::<T>(Some(wire_name), None);
    }

    /// Register a networked type. Called by the networking crate.
    ///
    /// The name is required: the wire index is its rank among every networked name, so a type
    /// without one has no place on the wire.
    ///
    /// # Panics
    ///
    /// If the type or the name was registered before, or the registry is already frozen.
    pub fn register_networked<T: TickedComponent>(&mut self, wire_name: &'static str, wire: WireFns) {
        self.register_inner::<T>(Some(wire_name), Some(wire));
    }

    fn register_inner<T: TickedComponent>(
        &mut self,
        wire_name: Option<&'static str>,
        wire: Option<WireFns>,
    ) {
        let type_id = TypeId::of::<T>();
        let tname = type_name::<T>();
        assert!(
            self.inner.frozen.get().is_none(),
            "ticked component `{tname}` was registered after the wire format was frozen: every \
             networked registration has to happen before the first snapshot, handshake or \
             `wire_hash` — in a plugin's `build`, not at runtime"
        );
        let inner = Arc::make_mut(&mut self.inner);
        let wire_name = wire_name.unwrap_or(tname);

        if inner.type_indices.contains_key(&type_id) {
            panic!("Ticked component type `{tname}` was registered more than once");
        }
        if wire.is_some()
            && let Some(other) = inner
                .entries
                .iter()
                .find(|entry| entry.wire.is_some() && entry.wire_name == wire_name)
        {
            let _ = other;
            panic!(
                "two networked ticked components share the wire name `{wire_name}` (the second \
                 is `{tname}`); a wire name is a type's identity on the wire and must be unique"
            );
        }

        let next_index = u16::try_from(inner.entries.len()).unwrap_or_else(|_| {
            panic!(
                "Too many ticked component types registered: maximum is {}",
                u16::MAX
            )
        });

        inner.entries.push(RegisteredTickedComponent {
            wire_name,
            type_id,
            capture: capture_component::<T>,
            restore: restore_component::<T>,
            truncate_after: truncate_component::<T>,
            prune_before: prune_component::<T>,
            clear: clear_component::<T>,
            has_tick: has_tick_component::<T>,
            oldest_tick: oldest_tick_component::<T>,
            saved_ids: saved_ids_component::<T>,
            wire,
        });
        inner.type_indices.insert(type_id, next_index);
    }

    /// The registration index of a component type: its position in registration order.
    ///
    /// Not a wire index. It names the type inside this process and nowhere else; see
    /// [`wire_index_of`](Self::wire_index_of) for the one that travels.
    pub fn index_of<T: TickedComponent>(&self) -> Option<u16> {
        self.inner.type_indices.get(&TypeId::of::<T>()).copied()
    }

    /// The wire order, computed once. Freezes the registry.
    fn frozen(&self) -> &Frozen {
        self.inner.frozen.get_or_init(|| {
            let mut networked: Vec<(usize, &RegisteredTickedComponent)> = self
                .inner
                .entries
                .iter()
                .enumerate()
                .filter(|(_, entry)| entry.wire.is_some())
                .collect();
            networked.sort_by(|a, b| a.1.wire_name.cmp(b.1.wire_name));
            let names: Vec<&'static str> = networked.iter().map(|(_, e)| e.wire_name).collect();
            let hash = names.iter().fold(
                fnv_fold(FNV_OFFSET, &PROTOCOL_VERSION.to_le_bytes()),
                |hash, name| fnv_fold(fnv_fold(hash, name.as_bytes()), b"\0"),
            );
            Frozen {
                entries: networked.iter().map(|(i, _)| *i as u16).collect(),
                by_type: networked
                    .iter()
                    .enumerate()
                    .map(|(wire, (_, entry))| (entry.type_id, wire as u16))
                    .collect(),
                names,
                hash,
            }
        })
    }

    /// Whether the wire order has been computed, after which no registration is accepted.
    pub fn is_frozen(&self) -> bool {
        self.inner.frozen.get().is_some()
    }

    /// The wire index of a networked type: its rank among every networked type's name.
    ///
    /// Derived, not assigned, so two peers that registered the same names in any order agree.
    /// `None` for a rollback-only type, which never travels. Freezes the registry.
    pub fn wire_index_of<T: TickedComponent>(&self) -> Option<u16> {
        self.frozen().by_type.get(&TypeId::of::<T>()).copied()
    }

    /// How many types are on the wire. Freezes the registry.
    pub fn wire_len(&self) -> usize {
        self.frozen().entries.len()
    }

    fn wire_entry(&self, wire_index: u16) -> Option<(&RegisteredTickedComponent, &WireFns)> {
        let entry = &self.inner.entries[*self.frozen().entries.get(wire_index as usize)? as usize];
        entry.wire.as_ref().map(|wire| (entry, wire))
    }

    /// Every networked type's wire name, in wire order (sorted). Freezes the registry.
    ///
    /// The wire format is this list and nothing else: a snapshot names a type by its position
    /// here, and two peers with the same list agree about every byte. Registration order no
    /// longer matters; it used to be the format, and a reorder read one type's bytes as
    /// another's with no error of any kind.
    pub fn wire_names(&self) -> impl ExactSizeIterator<Item = &'static str> + '_ {
        self.frozen().names.iter().copied()
    }

    /// The name of every registered type, networked or not, in registration order. For logs.
    pub fn registered_names(&self) -> impl ExactSizeIterator<Item = &'static str> + '_ {
        self.inner.entries.iter().map(|entry| entry.wire_name)
    }

    /// A hash of [`PROTOCOL_VERSION`] and the sorted networked names. Freezes the registry.
    ///
    /// Exchange it with a peer at the join and compare: equal means the two wire formats are
    /// the same; unequal means the session must not start, and the sorted name lists say which
    /// registration differs. Rollback-only types do not enter it — they never travel, so a
    /// peer with an extra local-only type is a peer that agrees about every byte on the wire.
    pub fn wire_hash(&self) -> u64 {
        self.frozen().hash
    }

    /// Number of registered component types.
    pub fn len(&self) -> usize {
        self.inner.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.entries.is_empty()
    }

    /// The oldest tick any registered component still holds state for.
    pub fn oldest_captured_tick(&self, world: &World) -> Option<u64> {
        self.inner
            .entries
            .iter()
            .filter_map(|entry| (entry.oldest_tick)(world))
            .min()
    }

    /// Check if any registered component has captured state at the given tick.
    pub fn has_tick_captured(&self, world: &World, tick: u64) -> bool {
        self.inner
            .entries
            .iter()
            .any(|entry| (entry.has_tick)(world, tick))
    }

    /// Capture all registered components at the given tick — **and all registered resources**.
    ///
    /// The resource half rides along here rather than being a call of its own, and that is worth
    /// a sentence because it is surprising. There are fifteen places in this workspace that drive
    /// the component history, and a resource history that is driven from fourteen of them is worse
    /// than no resource history at all: it would be *nearly* right, and the tick it was wrong on
    /// would be a rollback that restored half a world. Riding along makes forgetting impossible,
    /// and it is the same reason both halves are captured at the same instant rather than by two
    /// systems that happen to be ordered.
    ///
    /// The same applies to [`restore_all`](Self::restore_all),
    /// [`truncate_all_after`](Self::truncate_all_after),
    /// [`prune_all_before`](Self::prune_all_before) and [`clear_all`](Self::clear_all).
    pub fn capture_all(&self, world: &mut World, tick: u64) {
        for entry in &self.inner.entries {
            (entry.capture)(world, tick);
        }
        if let Some(resources) = world.get_resource::<TickedResourceRegistry>().cloned() {
            resources.capture_all(world, tick);
        }
    }

    /// Restore all registered components from the given tick.
    ///
    /// Rewinding past a spawn leaves a **husk**, and this reports it. `restore_all`
    /// iterates entities that *currently* carry [`TickTrackedEntity`] and, per type,
    /// inserts the saved value or removes the component when that entity is absent
    /// from the tick's map. An entity spawned after the target tick is in no saved
    /// map at all, so every registered component is stripped from it — and nothing
    /// despawns it. What is left still carries its marker, its collider and its
    /// visuals, but none of its state, and nothing will ever put them back.
    ///
    /// The real fix is entity lifecycle in the history, so a rewind despawns what
    /// did not exist. Until then the least this can do is not be silent: a
    /// determinism harness that can quietly corrupt the world it is testing is the
    /// worst possible shape for an instrument.
    ///
    /// Reported rather than repaired, and deliberately not guessed at: an entity
    /// that legitimately carries none of the registered types at the target tick is
    /// indistinguishable from one that did not exist, so a
    /// despawn-what-has-no-state heuristic would kill it.
    pub fn restore_all(&self, world: &mut World, tick: u64) {
        for entry in &self.inner.entries {
            (entry.restore)(world, tick);
        }
        if let Some(resources) = world.get_resource::<TickedResourceRegistry>().cloned() {
            resources.restore_all(world, tick);
        }
        self.report_husks(world, tick);
    }

    /// Restore only the types that never travel: registered here without serialization, so
    /// no snapshot can carry them.
    ///
    /// A client applying an authoritative snapshot for `tick` gets every networked type from
    /// the wire, and used to get nothing for the rest: a rollback-only component kept whatever
    /// value the client's last predicted tick left in it, and the replay from `tick` started
    /// from a state that was half the authority's and half the client's future. This puts the
    /// local half back to what it was at `tick`, from history; the networked half is the
    /// snapshot's job. Husks are not reported: the snapshot's absence rule has already
    /// despawned what should not exist.
    pub fn restore_local_only(&self, world: &mut World, tick: u64) {
        for entry in &self.inner.entries {
            if entry.wire.is_none() {
                (entry.restore)(world, tick);
            }
        }
        if let Some(resources) = world.get_resource::<TickedResourceRegistry>().cloned() {
            resources.restore_local_only(world, tick);
        }
    }

    /// Warn about tracked entities with no saved state at `tick`.
    fn report_husks(&self, world: &mut World, tick: u64) {
        // Only meaningful if the tick was captured at all; restoring an unknown
        // tick is a no-op for every type, so nothing was stripped.
        if !self.has_tick_captured(world, tick) {
            return;
        }
        let present: Vec<(Entity, u64)> = {
            let mut tracked = world.query::<(Entity, &TickTrackedEntity)>();
            tracked.iter(world).map(|(e, t)| (e, t.0)).collect()
        };
        let saved: Vec<u64> = self
            .inner
            .entries
            .iter()
            .flat_map(|entry| (entry.saved_ids)(world, tick))
            .collect();

        let husks: Vec<u64> = present
            .iter()
            .map(|(_, net_id)| *net_id)
            .filter(|net_id| !saved.contains(net_id))
            .collect();
        if husks.is_empty() {
            return;
        }
        warn!(
            "rolled back to tick {tick} past the spawn of {} tracked {}: net {:?} \
             existed at no point in that tick's history, so every registered \
             component has just been stripped from them and nothing will put them \
             back. They are still tracked, still drawn, and now stateless.",
            husks.len(),
            if husks.len() == 1 { "entity" } else { "entities" },
            husks
        );
    }

    /// Truncate all WorldActions history after the given tick.
    pub fn truncate_all_after(&self, world: &mut World, tick: u64) {
        for entry in &self.inner.entries {
            (entry.truncate_after)(world, tick);
        }
        if let Some(resources) = world.get_resource::<TickedResourceRegistry>().cloned() {
            resources.truncate_all_after(world, tick);
        }
    }

    /// Remove all WorldActions history before the given tick.
    pub fn prune_all_before(&self, world: &mut World, tick: u64) {
        for entry in &self.inner.entries {
            (entry.prune_before)(world, tick);
        }
        if let Some(resources) = world.get_resource::<TickedResourceRegistry>().cloned() {
            resources.prune_all_before(world, tick);
        }
    }

    /// Clear all WorldActions history for all registered components.
    pub fn clear_all(&self, world: &mut World) {
        for entry in &self.inner.entries {
            (entry.clear)(world);
        }
        if let Some(resources) = world.get_resource::<TickedResourceRegistry>().cloned() {
            resources.clear_all(world);
        }
    }

    // ---- the wire, entity by entity --------------------------------------------------------

    /// Which networked types have a value saved for `(tick, id)`, as a wire-index mask.
    pub fn present_at(&self, world: &World, tick: u64, id: u64) -> TypeMask {
        let frozen = self.frozen();
        let mut mask = TypeMask::with_len(frozen.entries.len());
        for (wire_index, entry_index) in frozen.entries.iter().enumerate() {
            let entry = &self.inner.entries[*entry_index as usize];
            if let Some(wire) = &entry.wire
                && (wire.has_at)(world, tick, id)
            {
                mask.set(wire_index as u16);
            }
        }
        mask
    }

    /// Append the value of wire type `wire_index` saved for `(tick, id)` to `out`.
    pub fn encode_one(&self, world: &World, wire_index: u16, tick: u64, id: u64, out: &mut Vec<u8>) -> bool {
        match self.wire_entry(wire_index) {
            Some((_, wire)) => (wire.encode_one)(world, tick, id, out),
            None => false,
        }
    }

    /// Take one value of wire type `wire_index` from the front of `bytes`, insert it on `entity`
    /// and record it at `(tick, id)`. Returns the bytes consumed; `None` if the index is not on
    /// this peer's wire or the bytes did not decode.
    pub fn decode_one(
        &self,
        world: &mut World,
        wire_index: u16,
        tick: u64,
        entity: Entity,
        id: u64,
        bytes: &[u8],
    ) -> Option<usize> {
        let (_, wire) = self.wire_entry(wire_index)?;
        (wire.decode_one)(world, tick, entity, id, bytes)
    }

    /// Open a history entry at `tick` for every networked type, before decoding a snapshot's
    /// records into it.
    pub fn begin_wire_tick(&self, world: &mut World, tick: u64) {
        for wire_index in 0..self.wire_len() as u16 {
            if let Some((_, wire)) = self.wire_entry(wire_index) {
                (wire.begin_tick)(world, tick);
            }
        }
    }

    /// Close the history entry at `tick`: every tracked entity that the snapshot did not give a
    /// value of a networked type loses that type. Absence is authoritative.
    pub fn finish_wire_tick(&self, world: &mut World, tick: u64) {
        for wire_index in 0..self.wire_len() as u16 {
            if let Some((_, wire)) = self.wire_entry(wire_index) {
                (wire.finish_tick)(world, tick);
            }
        }
    }

    /// The name of wire type `wire_index`, for an error message.
    pub fn wire_name_of(&self, wire_index: u16) -> Option<&'static str> {
        self.frozen().names.get(wire_index as usize).copied()
    }
}

/// Which wire types an entity record carries, one bit per wire index.
///
/// Sent instead of a list of indices because nearly every record carries the same few types,
/// and a mask of them is one or two bytes where a list is one per type.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct TypeMask(pub Vec<u64>);

impl TypeMask {
    /// An empty mask with room for `len` wire indices.
    pub fn with_len(len: usize) -> Self {
        Self(vec![0; len.div_ceil(64)])
    }

    pub fn set(&mut self, wire_index: u16) {
        let (word, bit) = (wire_index as usize / 64, wire_index as usize % 64);
        if word >= self.0.len() {
            self.0.resize(word + 1, 0);
        }
        self.0[word] |= 1 << bit;
    }

    pub fn contains(&self, wire_index: u16) -> bool {
        let (word, bit) = (wire_index as usize / 64, wire_index as usize % 64);
        self.0.get(word).is_some_and(|w| w & (1 << bit) != 0)
    }

    pub fn is_empty(&self) -> bool {
        self.0.iter().all(|w| *w == 0)
    }

    /// Every set wire index, ascending.
    pub fn iter(&self) -> impl Iterator<Item = u16> + '_ {
        self.0.iter().enumerate().flat_map(|(word, bits)| {
            (0..64u16)
                .filter(move |bit| bits & (1u64 << bit) != 0)
                .map(move |bit| word as u16 * 64 + bit)
        })
    }
}

/// Extension trait for registering ticked components on the App.
pub trait TickedAppExt {
    /// Register a component for tick-based state tracking.
    ///
    /// Rolled back, never sent. The type contributes its *position* to
    /// [`wire_hash`](TickedComponentRegistry::wire_hash) but not its name — see
    /// [`register_ticked_component_as`](Self::register_ticked_component_as) for why that is worth
    /// changing on anything long-lived.
    fn register_ticked_component<T: TickedComponent>(&mut self) -> &mut Self;

    /// As [`register_ticked_component`](Self::register_ticked_component), with a stable wire name.
    ///
    /// The networked path has had this since the registration handshake existed, and the
    /// rollback-only path did not — so the one kind of type that *could not* opt out of
    /// `std::any::type_name` was hashed under it. That is why `wire_hash` folds a sentinel for an
    /// unnamed entry rather than its name, and why anything meant to outlive a single build should
    /// be registered through here instead: a named entry makes the handshake able to say *which*
    /// registration differs, rather than only that the shapes do.
    fn register_ticked_component_as<T: TickedComponent>(
        &mut self,
        wire_name: &'static str,
    ) -> &mut Self;
}

impl TickedAppExt for App {
    fn register_ticked_component<T: TickedComponent>(&mut self) -> &mut Self {
        self.init_resource::<TickedComponentRegistry>();
        self.init_resource::<WorldActions<T>>();
        let mut registry = self.world_mut().resource_mut::<TickedComponentRegistry>();
        registry.register::<T>();
        self
    }

    fn register_ticked_component_as<T: TickedComponent>(
        &mut self,
        wire_name: &'static str,
    ) -> &mut Self {
        self.init_resource::<TickedComponentRegistry>();
        self.init_resource::<WorldActions<T>>();
        let mut registry = self.world_mut().resource_mut::<TickedComponentRegistry>();
        registry.register_as::<T>(wire_name);
        self
    }
}

// --- Type-erased dispatch functions ---

fn capture_component<T: TickedComponent>(world: &mut World, tick: u64) {
    // A recycled map and the cached query: after warm-up neither line allocates, and nothing
    // below does either, unless `T::clone` does. See `WorldActions` for why that matters.
    let (mut state, query) = {
        let mut actions = world.resource_mut::<WorldActions<T>>();
        (actions.take_map(), actions.take_query())
    };
    let mut query = query.unwrap_or_else(|| world.query::<(&TickTrackedEntity, &T)>());

    for (net_id, component) in query.iter(world) {
        state.insert(net_id.0, component.clone());
    }

    let mut actions = world.resource_mut::<WorldActions<T>>();
    actions.set_tick(tick, state);
    actions.put_query(query);
}

fn restore_component<T: TickedComponent>(world: &mut World, tick: u64) {
    if world.resource::<WorldActions<T>>().at_tick(tick).is_none() {
        return;
    }

    let mut query = world.query::<(Entity, &TickTrackedEntity)>();
    let entity_map: Vec<(Entity, u64)> = query
        .iter(world)
        .map(|(entity, net_id)| (entity, net_id.0))
        .collect();

    // One component cloned at a time, rather than the whole tick's map cloned up front: the
    // saved map is read between two mutations of the world, and cloning it was an allocation
    // the size of the world on every rollback.
    for (entity, net_id) in &entity_map {
        let saved = world
            .resource::<WorldActions<T>>()
            .at_tick(tick)
            .and_then(|state| state.get(net_id))
            .cloned();
        if let Some(component) = saved {
            world.entity_mut(*entity).insert(component);
        } else {
            world.entity_mut(*entity).remove::<T>();
        }
    }
}

fn truncate_component<T: TickedComponent>(world: &mut World, tick: u64) {
    world
        .resource_mut::<WorldActions<T>>()
        .truncate_after(tick);
}

fn prune_component<T: TickedComponent>(world: &mut World, tick: u64) {
    world
        .resource_mut::<WorldActions<T>>()
        .prune_before(tick);
}

fn clear_component<T: TickedComponent>(world: &mut World) {
    world.resource_mut::<WorldActions<T>>().clear();
}

fn saved_ids_component<T: TickedComponent>(world: &World, tick: u64) -> Vec<u64> {
    world
        .resource::<WorldActions<T>>()
        .at_tick(tick)
        .map(|state| state.keys().copied().collect())
        .unwrap_or_default()
}

fn oldest_tick_component<T: TickedComponent>(world: &World) -> Option<u64> {
    world.resource::<WorldActions<T>>().oldest_recorded_tick()
}

fn has_tick_component<T: TickedComponent>(world: &World, tick: u64) -> bool {
    world
        .resource::<WorldActions<T>>()
        .at_tick(tick)
        .is_some()
}
