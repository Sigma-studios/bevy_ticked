use std::{
    any::type_name,
    collections::HashMap,
};

use bevy::prelude::*;
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
    fn register_networked_ticked_component<T: NetworkedTickedComponent>(&mut self) -> &mut Self;
}

impl NetworkedTickedAppExt for App {
    fn register_networked_ticked_component<T: NetworkedTickedComponent>(&mut self) -> &mut Self {
        self.init_resource::<TickedComponentRegistry>();
        self.init_resource::<WorldActions<T>>();
        let mut registry = self.world_mut().resource_mut::<TickedComponentRegistry>();
        registry.register_with_serialization::<T>(
            serialize_component::<T>,
            deserialize_and_apply_component::<T>,
            deserialize_and_insert_one_component::<T>,
        );
        self
    }
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
        let bytes = postcard::to_allocvec(component).unwrap_or_else(|error| {
            panic!(
                "Failed to serialize ticked component `{}`: {error}",
                type_name::<T>()
            )
        });
        result.insert(*net_id, bytes);
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
        let component: T = postcard::from_bytes(bytes).unwrap_or_else(|error| {
            panic!(
                "Failed to deserialize ticked component `{}`: {error}",
                type_name::<T>()
            )
        });
        state.insert(*net_id, component);
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
}

fn deserialize_and_insert_one_component<T: NetworkedTickedComponent>(
    world: &mut World,
    entity: Entity,
    bytes: &[u8],
) {
    let component: T = postcard::from_bytes(bytes).unwrap_or_else(|error| {
        panic!(
            "Failed to deserialize ticked component `{}`: {error}",
            type_name::<T>()
        )
    });
    world.entity_mut(entity).insert(component);
}
