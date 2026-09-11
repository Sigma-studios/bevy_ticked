use std::{
    any::{TypeId, type_name},
    collections::HashMap,
    sync::Arc,
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

/// What an entry with no explicit wire name contributes instead of its `type_name`.
pub(crate) const UNNAMED_SENTINEL: &[u8] = b"<unnamed>";

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
}

#[derive(Clone)]
struct RegisteredTickedComponent {
    /// The name this type travels under in the registration handshake.
    ///
    /// Defaults to `type_name::<T>()`, and anything networked should override it.
    /// `std::any::type_name`'s output is explicitly not guaranteed stable across
    /// compiler versions, so hashing it makes "our registries disagree" fire on a
    /// rustc upgrade — a loud error for a non-problem, which is how a check gets
    /// ignored within a week.
    wire_name: &'static str,
    /// Whether [`wire_name`](Self::wire_name) was given at registration or defaulted to
    /// `type_name`. Only an explicit one may enter [`TickedComponentRegistry::wire_hash`].
    named: bool,
    capture: fn(&mut World, u64),
    restore: fn(&mut World, u64),
    truncate_after: fn(&mut World, u64),
    prune_before: fn(&mut World, u64),
    clear: fn(&mut World),
    has_tick: fn(&World, u64) -> bool,
    oldest_tick: fn(&World) -> Option<u64>,
    /// Which tracked-entity ids this type has saved state for at a tick.
    saved_ids: fn(&World, u64) -> Vec<u64>,
    /// Optional serialization support, populated by the networking crate.
    serialize_at: Option<fn(&mut World, u64) -> Option<HashMap<u64, Vec<u8>>>>,
    deserialize_and_apply: Option<fn(&mut World, u64, &HashMap<u64, Vec<u8>>)>,
    /// Optional: deserialize and insert a single component onto a specific entity.
    deserialize_and_insert_one: Option<fn(&mut World, Entity, &[u8])>,
}

impl TickedComponentRegistry {
    pub fn register<T: TickedComponent>(&mut self) {
        self.register_inner::<T>(None, None, None, None);
    }

    /// Register for rollback with a stable wire name. See
    /// [`TickedAppExt::register_ticked_component_as`].
    pub fn register_as<T: TickedComponent>(&mut self, wire_name: &'static str) {
        self.register_inner::<T>(Some(wire_name), None, None, None);
    }

    /// Register with serialization support. Called by the networking crate.
    pub fn register_with_serialization<T: TickedComponent>(
        &mut self,
        wire_name: Option<&'static str>,
        serialize_at: fn(&mut World, u64) -> Option<HashMap<u64, Vec<u8>>>,
        deserialize_and_apply: fn(&mut World, u64, &HashMap<u64, Vec<u8>>),
        deserialize_and_insert_one: fn(&mut World, Entity, &[u8]),
    ) {
        self.register_inner::<T>(
            wire_name,
            Some(serialize_at),
            Some(deserialize_and_apply),
            Some(deserialize_and_insert_one),
        );
    }

    fn register_inner<T: TickedComponent>(
        &mut self,
        wire_name: Option<&'static str>,
        serialize_at: Option<fn(&mut World, u64) -> Option<HashMap<u64, Vec<u8>>>>,
        deserialize_and_apply: Option<fn(&mut World, u64, &HashMap<u64, Vec<u8>>)>,
        deserialize_and_insert_one: Option<fn(&mut World, Entity, &[u8])>,
    ) {
        let inner = Arc::make_mut(&mut self.inner);
        let type_id = TypeId::of::<T>();
        let tname = type_name::<T>();
        let named = wire_name.is_some();
        let wire_name = wire_name.unwrap_or(tname);

        if inner.type_indices.contains_key(&type_id) {
            panic!("Ticked component type `{tname}` was registered more than once");
        }

        let next_index = u16::try_from(inner.entries.len()).unwrap_or_else(|_| {
            panic!(
                "Too many ticked component types registered: maximum is {}",
                u16::MAX
            )
        });

        inner.entries.push(RegisteredTickedComponent {
            wire_name,
            named,
            capture: capture_component::<T>,
            restore: restore_component::<T>,
            truncate_after: truncate_component::<T>,
            prune_before: prune_component::<T>,
            clear: clear_component::<T>,
            has_tick: has_tick_component::<T>,
            oldest_tick: oldest_tick_component::<T>,
            saved_ids: saved_ids_component::<T>,
            serialize_at,
            deserialize_and_apply,
            deserialize_and_insert_one,
        });
        inner.type_indices.insert(type_id, next_index);
    }

    /// Get the index for a component type.
    pub fn index_of<T: TickedComponent>(&self) -> Option<u16> {
        self.inner.type_indices.get(&TypeId::of::<T>()).copied()
    }

    /// Every registered type's wire name, in registration order.
    ///
    /// That order **is** the wire format: indices are assigned by position as
    /// `u16` and travel in every snapshot, so two peers that registered in
    /// different orders read each other's `Position` bytes as something else
    /// entirely, with no error of any kind.
    pub fn wire_names(&self) -> impl ExactSizeIterator<Item = &'static str> + '_ {
        self.inner.entries.iter().map(|entry| entry.wire_name)
    }

    /// A hash of `(index, wire_name)` for every registered type.
    ///
    /// Exchange this with a peer at join time and compare. Equal means the two
    /// registries agree; unequal means every component index from the first
    /// difference onward means something different on the other machine, and the
    /// session should not start.
    ///
    /// FNV-1a over the names with their positions folded in, so a reorder changes
    /// the hash even though the multiset of names has not.
    ///
    /// # Why an unnamed type contributes its position but not its name
    ///
    /// [`register_ticked_component`] defaults `wire_name` to `std::any::type_name`, whose output
    /// is explicitly not specified across compiler versions. Folding that in would make two peers
    /// built on different rustc releases report disagreeing registries when they agree perfectly
    /// — a loud error for a non-problem, which is how a check gets switched off.
    ///
    /// Skipping such entries outright would be worse than useless: an extra unnamed type on one
    /// peer shifts every index after it, which is exactly the corruption this exists to catch, and
    /// the hashes would still match. So an unnamed entry folds its index and a fixed sentinel.
    /// **Position always counts; an unstable string never does.**
    ///
    /// Give a rollback-only type a stable name with [`register_ticked_component_as`] and it
    /// contributes properly.
    ///
    /// [`register_ticked_component`]: TickedAppExt::register_ticked_component
    /// [`register_ticked_component_as`]: TickedAppExt::register_ticked_component_as
    pub fn wire_hash(&self) -> u64 {
        self.inner
            .entries
            .iter()
            .enumerate()
            .fold(FNV_OFFSET, |hash, (index, entry)| {
                let hash = fnv_fold(hash, &(index as u16).to_le_bytes());
                if entry.named {
                    fnv_fold(hash, entry.wire_name.as_bytes())
                } else {
                    fnv_fold(hash, UNNAMED_SENTINEL)
                }
            })
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
            if entry.serialize_at.is_none() {
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

    /// Serialize all registered components at the given tick.
    /// Only includes components that were registered with serialization support.
    pub fn serialize_all(
        &self,
        world: &mut World,
        tick: u64,
    ) -> HashMap<u16, HashMap<u64, Vec<u8>>> {
        let mut result = HashMap::new();
        for (i, entry) in self.inner.entries.iter().enumerate() {
            if let Some(serialize_fn) = entry.serialize_at {
                if let Some(data) = serialize_fn(world, tick) {
                    result.insert(i as u16, data);
                }
            }
        }
        result
    }

    /// Deserialize and apply snapshot data for all component types at the given tick.
    pub fn deserialize_and_apply_all(
        &self,
        world: &mut World,
        tick: u64,
        components: &HashMap<u16, HashMap<u64, Vec<u8>>>,
    ) {
        for (index, data) in components {
            if let Some(entry) = self.inner.entries.get(*index as usize) {
                if let Some(deserialize_fn) = entry.deserialize_and_apply {
                    deserialize_fn(world, tick, data);
                }
            }
        }
    }

    /// Deserialize a single component from bytes and insert it onto a specific entity.
    /// Returns false if the type index has no serialization support.
    pub fn deserialize_and_insert_one(
        &self,
        world: &mut World,
        type_index: u16,
        entity: Entity,
        bytes: &[u8],
    ) -> bool {
        if let Some(entry) = self.inner.entries.get(type_index as usize) {
            if let Some(f) = entry.deserialize_and_insert_one {
                f(world, entity, bytes);
                return true;
            }
        }
        false
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
