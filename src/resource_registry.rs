//! Rollback and replication for resources, not just components.
//!
//! # What this is for
//!
//! Rollback restored components and nothing else, so every shared mutable fact in a game — the
//! round number, the score, who is holding the objective — had to be a *component*, which meant
//! inventing an entity to hang it on. Both consumers of this crate did exactly that and both wrote
//! the same house rule to go with it: "shared mutable state lives on the tracked world-state
//! singleton, never in a `Resource`." A rule everybody has to remember is a gap with a coat on.
//!
//! It also made [the snapshot](crate) worse in a way that is easy to miss. A singleton's
//! components are nearly all constant nearly all of the time, and they were re-serialised every
//! tick along with everything else, so the workaround for this entry was paying the cost of the
//! next one.
//!
//! # A separate index space, on purpose
//!
//! Resources are registered into their own registry with their own `u16` indices. Folding them
//! into [`TickedComponentRegistry`](crate::registry::TickedComponentRegistry) would have been
//! fewer types, and would have renumbered every consumer's existing component registrations —
//! which is a wire format. Two spaces that never renumber each other is the cheaper mistake.
//!
//! # There is no per-entity anything here
//!
//! A resource is one value. `ResourceActions<R>` is `tick -> R`, not `tick -> (id -> R)`, and the
//! absence that means "the authority removed this component" has no counterpart: a resource that
//! is not in the snapshot is a resource this peer keeps. Removing a resource across the wire is
//! not expressible and deliberately so — the shapes that want it (a round that has not started, an
//! objective nobody holds) are better served by a value that says so.

use std::{
    any::{type_name, TypeId},
    collections::{HashMap, VecDeque},
    sync::{Arc, OnceLock},
};

use bevy::prelude::*;

/// Trait bound for resources that can be tracked by the tick system.
///
/// `Default` is the bound that lets [`TickedResourceRegistry::reset_all`] exist: it is what a
/// registered resource goes back to when a session ends. A registered resource is session
/// state by definition — that is what registering it says — and session state has to have a
/// value that means "no session". A type with no sensible default is one whose value is not
/// session state, and should be a component on a tracked entity or not registered at all.
pub trait TickedResource: Resource + Clone + Default + Send + Sync + 'static {}

impl<R> TickedResource for R where R: Resource + Clone + Default + Send + Sync + 'static {}

/// The history of one registered resource type across ticks.
///
/// A ring in tick order rather than a map: a capture after warm-up pushes into capacity the
/// prune just freed and allocates nothing, which is the rule the component histories keep.
#[derive(Resource)]
pub struct ResourceActions<R: TickedResource> {
    history: VecDeque<(u64, R)>,
}

impl<R: TickedResource> Default for ResourceActions<R> {
    fn default() -> Self {
        Self {
            history: VecDeque::new(),
        }
    }
}

impl<R: TickedResource> ResourceActions<R> {
    fn position(&self, tick: u64) -> Result<usize, usize> {
        self.history.binary_search_by_key(&tick, |(at, _)| *at)
    }

    pub fn at_tick(&self, tick: u64) -> Option<&R> {
        self.position(tick)
            .ok()
            .map(|at| &self.history[at].1)
    }

    pub fn oldest_recorded_tick(&self) -> Option<u64> {
        self.history.front().map(|(tick, _)| *tick)
    }

    pub fn newest_recorded_tick(&self) -> Option<u64> {
        self.history.back().map(|(tick, _)| *tick)
    }

    pub fn set_tick(&mut self, tick: u64, value: R) {
        match self.position(tick) {
            Ok(at) => self.history[at].1 = value,
            Err(at) if at == self.history.len() => self.history.push_back((tick, value)),
            Err(at) => self.history.insert(at, (tick, value)),
        }
    }

    pub fn truncate_after(&mut self, tick: u64) {
        let keep = match self.position(tick) {
            Ok(at) => at + 1,
            Err(at) => at,
        };
        self.history.truncate(keep);
    }

    pub fn prune_before(&mut self, tick: u64) {
        while self
            .history
            .front()
            .is_some_and(|(at, _)| *at < tick)
        {
            self.history.pop_front();
        }
    }

    pub fn clear(&mut self) {
        self.history.clear();
    }
}

/// Runtime registry mapping resource types to compact indices.
///
/// Independent of the component registry's index space — see the module note.
#[derive(Resource, Clone, Default)]
pub struct TickedResourceRegistry {
    inner: Arc<ResourceRegistryInner>,
}

#[derive(Default, Clone)]
struct ResourceRegistryInner {
    entries: Vec<RegisteredTickedResource>,
    type_indices: HashMap<TypeId, u16>,
    frozen: OnceLock<FrozenResources>,
}

/// The resource wire order: networked entries ranked by name. See
/// [`TickedComponentRegistry`](crate::registry::TickedComponentRegistry) for the rule.
#[derive(Clone, Debug, Default)]
struct FrozenResources {
    entries: Vec<u16>,
    by_type: HashMap<TypeId, u16>,
    names: Vec<&'static str>,
    hash: u64,
}

#[derive(Clone)]
struct RegisteredTickedResource {
    wire_name: &'static str,
    /// Whether the name was given at registration or defaulted to `type_name`. See
    /// [`TickedComponentRegistry::wire_hash`](crate::registry::TickedComponentRegistry::wire_hash).
    type_id: TypeId,
    capture: fn(&mut World, u64),
    restore: fn(&mut World, u64),
    truncate_after: fn(&mut World, u64),
    prune_before: fn(&mut World, u64),
    clear: fn(&mut World),
    reset: fn(&mut World),
    serialize_at: Option<fn(&World, u64) -> Option<Vec<u8>>>,
    deserialize_and_apply: Option<fn(&mut World, u64, &[u8])>,
}

impl TickedResourceRegistry {
    pub fn register<R: TickedResource>(&mut self) {
        self.register_inner::<R>(None, None, None);
    }

    /// Register with a stable name, rollback only.
    pub fn register_as<R: TickedResource>(&mut self, wire_name: &'static str) {
        self.register_inner::<R>(Some(wire_name), None, None);
    }

    /// Register a networked resource. Called by the networking crate. The name is required: it
    /// is the resource's identity on the wire.
    ///
    /// # Panics
    ///
    /// If the type or the name was registered before, or the registry is already frozen.
    pub fn register_networked<R: TickedResource>(
        &mut self,
        wire_name: &'static str,
        serialize_at: fn(&World, u64) -> Option<Vec<u8>>,
        deserialize_and_apply: fn(&mut World, u64, &[u8]),
    ) {
        self.register_inner::<R>(Some(wire_name), Some(serialize_at), Some(deserialize_and_apply));
    }

    fn register_inner<R: TickedResource>(
        &mut self,
        wire_name: Option<&'static str>,
        serialize_at: Option<fn(&World, u64) -> Option<Vec<u8>>>,
        deserialize_and_apply: Option<fn(&mut World, u64, &[u8])>,
    ) {
        let type_id = TypeId::of::<R>();
        let tname = type_name::<R>();
        assert!(
            self.inner.frozen.get().is_none(),
            "ticked resource `{tname}` was registered after the wire format was frozen; register \
             in a plugin's `build`, before the first snapshot or handshake"
        );
        let inner = Arc::make_mut(&mut self.inner);
        let wire_name = wire_name.unwrap_or(tname);

        if let Some(index) = inner.type_indices.get(&type_id).copied() {
            // Already rolled back; now networked too. The core registers its own resources
            // (the id allocator) for rollback, and the networking crate puts them on the
            // wire: one upgrade, never a second registration of a networked type.
            let entry = &mut inner.entries[index as usize];
            if entry.serialize_at.is_some() || serialize_at.is_none() {
                panic!("Ticked resource type `{tname}` was registered more than once");
            }
            entry.wire_name = wire_name;
            entry.serialize_at = serialize_at;
            entry.deserialize_and_apply = deserialize_and_apply;
            return;
        }
        if serialize_at.is_some()
            && inner
                .entries
                .iter()
                .any(|entry| entry.serialize_at.is_some() && entry.wire_name == wire_name)
        {
            panic!(
                "two networked ticked resources share the wire name `{wire_name}` (the second is \
                 `{tname}`); a wire name must be unique"
            );
        }

        let next_index = u16::try_from(inner.entries.len()).unwrap_or_else(|_| {
            panic!(
                "Too many ticked resource types registered: maximum is {}",
                u16::MAX
            )
        });

        inner.entries.push(RegisteredTickedResource {
            wire_name,
            type_id,
            capture: capture_resource::<R>,
            restore: restore_resource::<R>,
            truncate_after: truncate_resource::<R>,
            prune_before: prune_resource::<R>,
            clear: clear_resource::<R>,
            reset: reset_resource::<R>,
            serialize_at,
            deserialize_and_apply,
        });
        inner.type_indices.insert(type_id, next_index);
    }

    /// The registration index: position in registration order, meaningful in this process only.
    pub fn index_of<R: TickedResource>(&self) -> Option<u16> {
        self.inner.type_indices.get(&TypeId::of::<R>()).copied()
    }

    fn frozen(&self) -> &FrozenResources {
        self.inner.frozen.get_or_init(|| {
            let mut networked: Vec<(usize, &RegisteredTickedResource)> = self
                .inner
                .entries
                .iter()
                .enumerate()
                .filter(|(_, entry)| entry.serialize_at.is_some())
                .collect();
            networked.sort_by(|a, b| a.1.wire_name.cmp(b.1.wire_name));
            let names: Vec<&'static str> = networked.iter().map(|(_, e)| e.wire_name).collect();
            let hash = names.iter().fold(
                crate::registry::fnv_fold(
                    crate::registry::FNV_OFFSET,
                    &crate::registry::PROTOCOL_VERSION.to_le_bytes(),
                ),
                |hash, name| {
                    crate::registry::fnv_fold(crate::registry::fnv_fold(hash, name.as_bytes()), b"\0")
                },
            );
            FrozenResources {
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

    /// Whether a networked resource is registered under `wire_name`, without freezing.
    pub fn wire_names_unfrozen_contains(&self, wire_name: &str) -> bool {
        self.inner
            .entries
            .iter()
            .any(|entry| entry.serialize_at.is_some() && entry.wire_name == wire_name)
    }

    /// The wire index of a networked resource: its rank among the networked names. Freezes.
    pub fn wire_index_of<R: TickedResource>(&self) -> Option<u16> {
        self.frozen().by_type.get(&TypeId::of::<R>()).copied()
    }

    /// Every networked resource's wire name, in wire order (sorted). Freezes the registry.
    pub fn wire_names(&self) -> impl ExactSizeIterator<Item = &'static str> + '_ {
        self.frozen().names.iter().copied()
    }

    /// A hash of the protocol version and the sorted networked names, separate from the
    /// component one: the two index spaces are independent and a handshake compares both.
    pub fn wire_hash(&self) -> u64 {
        self.frozen().hash
    }

    pub fn len(&self) -> usize {
        self.inner.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.entries.is_empty()
    }

    pub fn capture_all(&self, world: &mut World, tick: u64) {
        for entry in &self.inner.entries {
            (entry.capture)(world, tick);
        }
    }

    pub fn restore_all(&self, world: &mut World, tick: u64) {
        for entry in &self.inner.entries {
            (entry.restore)(world, tick);
        }
    }

    /// Restore only the resources registered without serialization. See
    /// [`TickedComponentRegistry::restore_local_only`](crate::registry::TickedComponentRegistry::restore_local_only).
    pub fn restore_local_only(&self, world: &mut World, tick: u64) {
        for entry in &self.inner.entries {
            if entry.serialize_at.is_none() {
                (entry.restore)(world, tick);
            }
        }
    }

    pub fn truncate_all_after(&self, world: &mut World, tick: u64) {
        for entry in &self.inner.entries {
            (entry.truncate_after)(world, tick);
        }
    }

    pub fn prune_all_before(&self, world: &mut World, tick: u64) {
        for entry in &self.inner.entries {
            (entry.prune_before)(world, tick);
        }
    }

    pub fn clear_all(&self, world: &mut World) {
        for entry in &self.inner.entries {
            (entry.clear)(world);
        }
    }

    /// Put every registered resource back to `R::default()` and forget its history.
    ///
    /// Called when a session ends, by the networking crate's leave path. Registering a resource
    /// says it is part of the session, and the previous session's last value — the round that
    /// was on, the score, who held the objective — must not be the first thing the next lobby
    /// sees. Every consumer with a `RoundState` wrote a `reset_round_on_leave` system for exactly
    /// this; now none of them has to.
    ///
    /// A resource that is registered but not currently in the world is left absent: absence is
    /// a state the game chose, and inserting a default behind its back would be a change it
    /// never asked for. A resource that is present is overwritten, whatever it held.
    pub fn reset_all(&self, world: &mut World) {
        for entry in &self.inner.entries {
            (entry.reset)(world);
        }
    }

    /// Serialize every networked resource at `tick`, as `(wire index, bytes)` in wire order.
    pub fn serialize_all(&self, world: &World, tick: u64) -> Vec<(u16, Vec<u8>)> {
        let frozen = self.frozen();
        let mut result = Vec::new();
        for (wire_index, entry_index) in frozen.entries.iter().enumerate() {
            let entry = &self.inner.entries[*entry_index as usize];
            if let Some(serialize) = entry.serialize_at
                && let Some(bytes) = serialize(world, tick)
            {
                result.push((wire_index as u16, bytes));
            }
        }
        result
    }

    /// Apply serialized resource state, by wire index. Unknown indices are skipped.
    pub fn deserialize_and_apply_all(
        &self,
        world: &mut World,
        tick: u64,
        resources: &[(u16, Vec<u8>)],
    ) {
        let frozen = self.frozen();
        for (wire_index, bytes) in resources {
            let Some(entry_index) = frozen.entries.get(*wire_index as usize) else {
                continue;
            };
            if let Some(apply) = self.inner.entries[*entry_index as usize].deserialize_and_apply {
                apply(world, tick, bytes);
            }
        }
    }
}

fn capture_resource<R: TickedResource>(world: &mut World, tick: u64) {
    let Some(value) = world.get_resource::<R>().cloned() else {
        return;
    };
    world
        .get_resource_or_insert_with(ResourceActions::<R>::default)
        .set_tick(tick, value);
}

fn restore_resource<R: TickedResource>(world: &mut World, tick: u64) {
    let saved = world
        .get_resource::<ResourceActions<R>>()
        .and_then(|actions| actions.at_tick(tick).cloned());
    // A tick with nothing saved is a tick this resource did not exist at, or one outside the
    // window. Either way the last thing to do is guess: leave what is there.
    if let Some(value) = saved {
        world.insert_resource(value);
    }
}

fn truncate_resource<R: TickedResource>(world: &mut World, tick: u64) {
    if let Some(mut actions) = world.get_resource_mut::<ResourceActions<R>>() {
        actions.truncate_after(tick);
    }
}

fn prune_resource<R: TickedResource>(world: &mut World, tick: u64) {
    if let Some(mut actions) = world.get_resource_mut::<ResourceActions<R>>() {
        actions.prune_before(tick);
    }
}

fn clear_resource<R: TickedResource>(world: &mut World) {
    if let Some(mut actions) = world.get_resource_mut::<ResourceActions<R>>() {
        actions.clear();
    }
}

fn reset_resource<R: TickedResource>(world: &mut World) {
    // Inserted rather than assigned through `get_resource_mut`, which an immutable resource
    // cannot offer; a present resource is replaced either way.
    if world.contains_resource::<R>() {
        world.insert_resource(R::default());
    }
    clear_resource::<R>(world);
}

/// Register a resource for rollback.
pub trait TickedResourceAppExt {
    /// Captured and rolled back, never sent.
    fn register_ticked_resource<R: TickedResource>(&mut self) -> &mut Self;
}

impl TickedResourceAppExt for App {
    fn register_ticked_resource<R: TickedResource>(&mut self) -> &mut Self {
        self.init_resource::<TickedResourceRegistry>();
        self.init_resource::<ResourceActions<R>>();
        self.world_mut()
            .resource_mut::<TickedResourceRegistry>()
            .register::<R>();
        self
    }
}
