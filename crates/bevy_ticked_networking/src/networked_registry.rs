use std::any::type_name;

use bevy::{log::error, prelude::*};
use serde::{Serialize, de::DeserializeOwned};

use bevy_ticked::{
    registry::{TickedComponent, TickedComponentRegistry, WireFns},
    resource_registry::{ResourceActions, TickedResource, TickedResourceRegistry},
    tracked_entity::TickTrackedEntity,
    world_actions::WorldActions,
};

/// Trait bound for components that can be tracked AND serialized over the network.
///
/// No `PartialEq`: a client compares what the authority sent with what it predicted, and
/// does so on the *encoded bytes*, which every networked type already produces. Bitwise on
/// floats, as it should be — a prediction off by an ulp is one that will drift, and the
/// replay is how it is put right — and open to foreign types (a character controller's
/// state from another crate) that never implemented equality. A type whose encoding is not
/// canonical for equal values (a hash map inside it) is compared as unequal and replayed,
/// which is slower and never wrong.
pub trait NetworkedTickedComponent: TickedComponent + Serialize + DeserializeOwned {}

impl<T> NetworkedTickedComponent for T where T: TickedComponent + Serialize + DeserializeOwned {}

/// Extension trait for registering networked ticked components.
pub trait NetworkedTickedAppExt {
    /// Register a component for rollback **and** the wire, under `wire_name`.
    ///
    /// The name is the type's identity on the wire: its index in every snapshot is its rank
    /// among all networked names, so registration order does not matter and two peers that
    /// register the same names agree about every byte. It must be the same string on every
    /// peer and must not change once a build is in anybody's hands; renaming the Rust type is
    /// free, changing this string is a wire break. The join handshake compares the sorted
    /// lists and says which name differs.
    ///
    /// There is no unnamed variant any more. It defaulted to `std::any::type_name`, which is
    /// explicitly not stable across compiler versions, and a wire index derived from an
    /// unstable string is a session that breaks on a rustc upgrade.
    ///
    /// # Panics
    ///
    /// If the type or the name is already registered, or the registry has been frozen by a
    /// snapshot or handshake: register in a plugin's `build`.
    fn register_networked_ticked_component<T: NetworkedTickedComponent>(
        &mut self,
        wire_name: &'static str,
    ) -> &mut Self;
}

impl NetworkedTickedAppExt for App {
    fn register_networked_ticked_component<T: NetworkedTickedComponent>(
        &mut self,
        wire_name: &'static str,
    ) -> &mut Self {
        self.init_resource::<TickedComponentRegistry>();
        self.init_resource::<WorldActions<T>>();
        let mut registry = self.world_mut().resource_mut::<TickedComponentRegistry>();
        registry.register_networked::<T>(
            wire_name,
            WireFns {
                encode_one: encode_one::<T>,
                decode_one: decode_one::<T>,
                insert_one: insert_one::<T>,
                matches_at: matches_at::<T>,
                begin_tick: begin_tick::<T>,
                finish_tick: finish_tick::<T>,
                has_at: has_at::<T>,
            },
        );
        self
    }
}

/// Trait bound for resources that can be rolled back AND serialized over the network.
pub trait NetworkedTickedResource: TickedResource + Serialize + DeserializeOwned {}

impl<R> NetworkedTickedResource for R where R: TickedResource + Serialize + DeserializeOwned {}

/// Extension trait for registering networked ticked resources.
///
/// The point of these, and why the game-side shape they replace was a workaround: a fact that
/// belongs to the *world* rather than to any entity — the round number, the score, whose turn it
/// is — had no way to be rolled back or replicated, so it had to be a component, so it needed an
/// entity to live on. Consumers invented a "world state singleton" for that and wrote a house rule
/// telling everybody to remember it.
pub trait NetworkedTickedResourceAppExt {
    /// Captured, rolled back **and** serialised into snapshots, under `wire_name`.
    ///
    /// Resources have their own wire index space, derived from their own sorted names, exactly
    /// as for components. The name is required for the same reason.
    fn register_networked_ticked_resource<R: NetworkedTickedResource>(
        &mut self,
        wire_name: &'static str,
    ) -> &mut Self;
}

impl NetworkedTickedResourceAppExt for App {
    fn register_networked_ticked_resource<R: NetworkedTickedResource>(
        &mut self,
        wire_name: &'static str,
    ) -> &mut Self {
        self.init_resource::<TickedResourceRegistry>();
        self.init_resource::<ResourceActions<R>>();
        let mut registry = self.world_mut().resource_mut::<TickedResourceRegistry>();
        registry.register_networked::<R>(
            wire_name,
            serialize_resource::<R>,
            deserialize_and_apply_resource::<R>,
        );
        self
    }
}

fn serialize_resource<R: NetworkedTickedResource>(world: &World, tick: u64) -> Option<Vec<u8>> {
    let value = world.get_resource::<ResourceActions<R>>()?.at_tick(tick)?;
    match postcard::to_allocvec(value) {
        Ok(bytes) => Some(bytes),
        Err(err) => {
            error!(
                "Failed to serialize ticked resource `{}`: {err}",
                type_name::<R>()
            );
            None
        }
    }
}

fn deserialize_and_apply_resource<R: NetworkedTickedResource>(
    world: &mut World,
    tick: u64,
    bytes: &[u8],
) {
    match postcard::from_bytes::<R>(bytes) {
        Ok(value) => {
            world
                .get_resource_or_insert_with(ResourceActions::<R>::default)
                .set_tick(tick, value.clone());
            world.insert_resource(value);
        }
        Err(err) => {
            error!(
                "Failed to deserialize ticked resource `{}`: {err}",
                type_name::<R>()
            );
        }
    }
}

// --- Wire dispatch, one value at a time ---

fn has_at<T: NetworkedTickedComponent>(world: &World, tick: u64, id: u64) -> bool {
    world
        .resource::<WorldActions<T>>()
        .at_tick(tick)
        .is_some_and(|state| state.contains_key(&id))
}

fn encode_one<T: NetworkedTickedComponent>(
    world: &World,
    tick: u64,
    id: u64,
    out: &mut Vec<u8>,
) -> bool {
    let Some(value) = world
        .resource::<WorldActions<T>>()
        .at_tick(tick)
        .and_then(|state| state.get(&id))
    else {
        return false;
    };
    match postcard::to_extend(value, std::mem::take(out)) {
        Ok(extended) => {
            *out = extended;
            true
        }
        Err(err) => {
            error!(
                "Failed to serialize ticked component `{}` for entity {id}: {err}",
                type_name::<T>()
            );
            false
        }
    }
}

fn decode_one<T: NetworkedTickedComponent>(
    world: &mut World,
    tick: u64,
    entity: Entity,
    id: u64,
    bytes: &[u8],
) -> Option<usize> {
    match postcard::take_from_bytes::<T>(bytes) {
        Ok((value, rest)) => {
            let consumed = bytes.len() - rest.len();
            world
                .resource_mut::<WorldActions<T>>()
                .insert(tick, id, value.clone());
            world.entity_mut(entity).insert(value);
            Some(consumed)
        }
        Err(err) => {
            error!(
                "Failed to deserialize ticked component `{}` for entity {id}: {err}",
                type_name::<T>()
            );
            None
        }
    }
}

fn insert_one<T: NetworkedTickedComponent>(
    world: &mut World,
    entity: Entity,
    bytes: &[u8],
) -> Option<usize> {
    let (value, rest) = postcard::take_from_bytes::<T>(bytes).ok()?;
    let consumed = bytes.len() - rest.len();
    world.get_entity_mut(entity).ok()?.insert(value);
    Some(consumed)
}

/// Equal means the saved value encodes to exactly the bytes at the front of `bytes`. On a
/// mismatch the caller stops walking the record, so `consumed` is only meaningful when equal.
fn matches_at<T: NetworkedTickedComponent>(
    world: &World,
    tick: u64,
    id: u64,
    bytes: &[u8],
) -> Option<(bool, usize)> {
    let saved = world
        .resource::<WorldActions<T>>()
        .at_tick(tick)
        .and_then(|state| state.get(&id))?;
    let encoded = postcard::to_allocvec(saved).ok()?;
    if bytes.starts_with(&encoded) {
        Some((true, encoded.len()))
    } else {
        Some((false, 0))
    }
}

fn begin_tick<T: NetworkedTickedComponent>(world: &mut World, tick: u64) {
    let mut actions = world.resource_mut::<WorldActions<T>>();
    let map = actions.take_map();
    actions.set_tick(tick, map);
}

/// Absence is authoritative. The snapshot named every entity that carries `T`; a tracked
/// entity that still carries it and was not named lost it on the authority, and no later
/// snapshot could ever say so.
///
/// One query filtered on `With<T>` per type: most tracked entities never carry most types,
/// and a remove per (entity, type) pair would cost `entities x types` archetype moves.
fn finish_tick<T: NetworkedTickedComponent>(world: &mut World, tick: u64) {
    let mut carriers = world.query_filtered::<(Entity, &TickTrackedEntity), With<T>>();
    let stale: Vec<Entity> = {
        let actions = world.resource::<WorldActions<T>>();
        let Some(state) = actions.at_tick(tick) else {
            return;
        };
        carriers
            .iter(world)
            .filter(|(_, tracked)| !state.contains_key(&tracked.0))
            .map(|(entity, _)| entity)
            .collect()
    };
    for entity in stale {
        world.entity_mut(entity).remove::<T>();
    }
}
