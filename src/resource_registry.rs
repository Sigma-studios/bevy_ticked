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
    collections::{BTreeMap, HashMap},
    sync::Arc,
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
#[derive(Resource)]
pub struct ResourceActions<R: TickedResource> {
    pub(crate) history: BTreeMap<u64, R>,
}

impl<R: TickedResource> Default for ResourceActions<R> {
    fn default() -> Self {
        Self {
            history: BTreeMap::new(),
        }
    }
}

impl<R: TickedResource> ResourceActions<R> {
    pub fn at_tick(&self, tick: u64) -> Option<&R> {
        self.history.get(&tick)
    }

    pub fn oldest_recorded_tick(&self) -> Option<u64> {
        self.history.keys().next().copied()
    }

    pub fn newest_recorded_tick(&self) -> Option<u64> {
        self.history.keys().next_back().copied()
    }

    pub fn set_tick(&mut self, tick: u64, value: R) {
        self.history.insert(tick, value);
    }

    pub fn truncate_after(&mut self, tick: u64) {
        self.history.split_off(&(tick + 1));
    }

    pub fn prune_before(&mut self, tick: u64) {
        let kept = self.history.split_off(&tick);
        self.history = kept;
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
}

#[derive(Clone)]
struct RegisteredTickedResource {
    wire_name: &'static str,
    /// Whether the name was given at registration or defaulted to `type_name`. See
    /// [`TickedComponentRegistry::wire_hash`](crate::registry::TickedComponentRegistry::wire_hash).
    named: bool,
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

    /// Register with serialization support. Called by the networking crate.
    pub fn register_with_serialization<R: TickedResource>(
        &mut self,
        wire_name: Option<&'static str>,
        serialize_at: fn(&World, u64) -> Option<Vec<u8>>,
        deserialize_and_apply: fn(&mut World, u64, &[u8]),
    ) {
        self.register_inner::<R>(wire_name, Some(serialize_at), Some(deserialize_and_apply));
    }

    fn register_inner<R: TickedResource>(
        &mut self,
        wire_name: Option<&'static str>,
        serialize_at: Option<fn(&World, u64) -> Option<Vec<u8>>>,
        deserialize_and_apply: Option<fn(&mut World, u64, &[u8])>,
    ) {
        let inner = Arc::make_mut(&mut self.inner);
        let type_id = TypeId::of::<R>();
        let tname = type_name::<R>();
        let named = wire_name.is_some();
        let wire_name = wire_name.unwrap_or(tname);

        if inner.type_indices.contains_key(&type_id) {
            panic!("Ticked resource type `{tname}` was registered more than once");
        }

        let next_index = u16::try_from(inner.entries.len()).unwrap_or_else(|_| {
            panic!(
                "Too many ticked resource types registered: maximum is {}",
                u16::MAX
            )
        });

        inner.entries.push(RegisteredTickedResource {
            wire_name,
            named,
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

    pub fn index_of<R: TickedResource>(&self) -> Option<u16> {
        self.inner.type_indices.get(&TypeId::of::<R>()).copied()
    }

    /// Every registered resource's wire name, in registration order.
    ///
    /// As with components, that order **is** a wire format.
    pub fn wire_names(&self) -> impl ExactSizeIterator<Item = &'static str> + '_ {
        self.inner.entries.iter().map(|entry| entry.wire_name)
    }

    /// A hash of `(index, wire_name)` for every registered resource.
    ///
    /// The same scheme as [`TickedComponentRegistry::wire_hash`], including the sentinel for an
    /// entry whose name defaulted to `type_name`, and **a separate number**: resources have their
    /// own index space, so a peer that agrees about components can still disagree here. A
    /// handshake that compared only one of the two would validate the half that changes least.
    ///
    /// [`TickedComponentRegistry::wire_hash`]: crate::registry::TickedComponentRegistry::wire_hash
    pub fn wire_hash(&self) -> u64 {
        self.inner
            .entries
            .iter()
            .enumerate()
            .fold(crate::registry::FNV_OFFSET, |hash, (index, entry)| {
                let hash = crate::registry::fnv_fold(hash, &(index as u16).to_le_bytes());
                if entry.named {
                    crate::registry::fnv_fold(hash, entry.wire_name.as_bytes())
                } else {
                    crate::registry::fnv_fold(hash, crate::registry::UNNAMED_SENTINEL)
                }
            })
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

    /// Serialize every registered resource at `tick`, by index.
    pub fn serialize_all(&self, world: &World, tick: u64) -> HashMap<u16, Vec<u8>> {
        let mut result = HashMap::new();
        for (index, entry) in self.inner.entries.iter().enumerate() {
            if let Some(serialize) = entry.serialize_at {
                if let Some(bytes) = serialize(world, tick) {
                    result.insert(index as u16, bytes);
                }
            }
        }
        result
    }

    /// Apply serialized resource state, by index.
    pub fn deserialize_and_apply_all(
        &self,
        world: &mut World,
        tick: u64,
        resources: &HashMap<u16, Vec<u8>>,
    ) {
        for (index, bytes) in resources {
            if let Some(entry) = self.inner.entries.get(*index as usize) {
                if let Some(apply) = entry.deserialize_and_apply {
                    apply(world, tick, bytes);
                }
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
