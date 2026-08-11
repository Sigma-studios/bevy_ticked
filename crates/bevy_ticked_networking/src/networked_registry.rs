use std::{any::type_name, collections::HashMap};

use bevy::{log::error, prelude::*};
use serde::{Serialize, de::DeserializeOwned};

use bevy_ticked::{
    tracked_entity::TickTrackedEntity,
    registry::{TickedComponent, TickedComponentRegistry},
    world_actions::WorldActions,
};

/// Trait bound for components that can be tracked AND serialized over the network.
pub trait NetworkedTickedComponent:
    TickedComponent + Serialize + DeserializeOwned
{
}

impl<T> NetworkedTickedComponent for T where
    T: TickedComponent + Serialize + DeserializeOwned
{
}

/// Extension trait for registering networked ticked components.
pub trait NetworkedTickedAppExt {
    /// Register a component for tick-based tracking with network serialization support.
    ///
    /// **The order of these calls is a wire format.** Indices are assigned by
    /// position as `u16` and travel in every snapshot; reorder two registrations
    /// and a peer reads one component's bytes as another's, with no error of any
    /// kind. Append only, never reorder, never delete.
    ///
    /// Prefer [`register_networked_ticked_component_as`] for anything long-lived:
    /// it gives the type a stable name for the join handshake, so two peers built
    /// from different commits find out at the join rather than an hour later.
    ///
    /// [`register_networked_ticked_component_as`]: Self::register_networked_ticked_component_as
    fn register_networked_ticked_component<T: NetworkedTickedComponent>(&mut self) -> &mut Self;

    /// As [`register_networked_ticked_component`], with an explicit wire name.
    ///
    /// The name is what [`TickedComponentRegistry::wire_hash`] hashes, so it must
    /// be the same string on every peer and must not change once a build is in
    /// anybody's hands. Without one the type's `type_name` is used, which is fine
    /// for a prototype and wrong for a shipped game: `std::any::type_name` is
    /// explicitly not stable across compiler versions, so a peer on a newer rustc
    /// could be reported as having a different registry when it does not.
    ///
    /// Renaming the Rust type is then free; changing this string is a wire break.
    ///
    /// [`register_networked_ticked_component`]: Self::register_networked_ticked_component
    fn register_networked_ticked_component_as<T: NetworkedTickedComponent>(
        &mut self,
        wire_name: &'static str,
    ) -> &mut Self;
}

impl NetworkedTickedAppExt for App {
    fn register_networked_ticked_component<T: NetworkedTickedComponent>(&mut self) -> &mut Self {
        register_networked::<T>(self, None)
    }

    fn register_networked_ticked_component_as<T: NetworkedTickedComponent>(
        &mut self,
        wire_name: &'static str,
    ) -> &mut Self {
        register_networked::<T>(self, Some(wire_name))
    }
}

fn register_networked<'a, T: NetworkedTickedComponent>(
    app: &'a mut App,
    wire_name: Option<&'static str>,
) -> &'a mut App {
    app.init_resource::<TickedComponentRegistry>();
    app.init_resource::<WorldActions<T>>();
    let mut registry = app.world_mut().resource_mut::<TickedComponentRegistry>();
    registry.register_with_serialization::<T>(
        wire_name,
        serialize_component::<T>,
        deserialize_and_apply_component::<T>,
        deserialize_and_insert_one_component::<T>,
    );
    app
}

// --- Serialization dispatch functions ---

fn serialize_component<T: NetworkedTickedComponent>(
    world: &mut World,
    tick: u64,
) -> Option<HashMap<u64, Vec<u8>>> {
    let actions = world.resource::<WorldActions<T>>();
    let state = actions.at_tick(tick)?;

    let mut result = HashMap::new();
    for (net_id, component) in state {
        match postcard::to_allocvec(component) {
            Ok(bytes) => {
                result.insert(*net_id, bytes);
            }
            Err(err) => {
                error!(
                    "Failed to serialize ticked component `{}` for entity {net_id}: {err}",
                    type_name::<T>()
                );
            }
        }
    }
    Some(result)
}

fn deserialize_and_apply_component<T: NetworkedTickedComponent>(
    world: &mut World,
    tick: u64,
    data: &HashMap<u64, Vec<u8>>,
) {
    let mut state: HashMap<u64, T> = HashMap::new();
    for (net_id, bytes) in data {
        match postcard::from_bytes::<T>(bytes) {
            Ok(component) => {
                state.insert(*net_id, component);
            }
            Err(err) => {
                error!(
                    "Failed to deserialize ticked component `{}` for entity {net_id}: {err}",
                    type_name::<T>()
                );
            }
        }
    }

    world
        .resource_mut::<WorldActions<T>>()
        .set_tick(tick, state.clone());

    let mut query = world.query::<(Entity, &TickTrackedEntity)>();
    let entity_map: Vec<(Entity, u64)> = query
        .iter(world)
        .map(|(entity, net_id)| (entity, net_id.0))
        .collect();

    for (entity, net_id) in &entity_map {
        if let Some(component) = state.get(net_id) {
            world.entity_mut(*entity).insert(component.clone());
        }
    }

    // Absence is authoritative. `capture_component` always calls `set_tick`, so a
    // type that appears in the snapshot with no entry for a tracked entity means
    // the authority does not have it -- not that it said nothing. Without this a
    // removal is never replicated and no later snapshot can correct it, and
    // `restore_component` already answers the same question the other way for the
    // local rollback path.
    //
    // Filtered on `With<T>` rather than folded into the loop above: most tracked
    // entities never carry most registered types, and `entity_mut().remove::<T>()`
    // on every (entity, type) pair would make applying a snapshot cost
    // `entities x types` archetype lookups instead of one query per type.
    let mut carriers = world.query_filtered::<(Entity, &TickTrackedEntity), With<T>>();
    let stale: Vec<Entity> = carriers
        .iter(world)
        .filter(|(_, net_id)| !state.contains_key(&net_id.0))
        .map(|(entity, _)| entity)
        .collect();
    for entity in stale {
        world.entity_mut(entity).remove::<T>();
    }
}

fn deserialize_and_insert_one_component<T: NetworkedTickedComponent>(
    world: &mut World,
    entity: Entity,
    bytes: &[u8],
) {
    match postcard::from_bytes::<T>(bytes) {
        Ok(component) => {
            world.entity_mut(entity).insert(component);
        }
        Err(err) => {
            error!(
                "Failed to deserialize ticked component `{}` for entity {entity}: {err}",
                type_name::<T>()
            );
        }
    }
}
