use std::{
    any::{TypeId, type_name},
    collections::HashMap,
};

use bevy::prelude::*;

use crate::{tracked_entity::TickTrackedEntity, world_actions::WorldActions};

/// Trait bound for components that can be tracked by the tick system.
///
/// Only requires Clone for capture/restore. Automatically implemented.
pub trait TickedComponent: Component + Clone + Send + Sync + 'static {}

impl<T> TickedComponent for T where T: Component + Clone + Send + Sync + 'static {}

/// Runtime registry mapping component types to compact indices.
///
/// Each registered component type is assigned a sequential `u16` index.
/// Registration order must be the same on all peers.
#[derive(Resource, Default, Clone)]
pub struct TickedComponentRegistry {
    entries: Vec<RegisteredTickedComponent>,
    type_indices: HashMap<TypeId, u16>,
}

#[derive(Clone)]
struct RegisteredTickedComponent {
    _type_name: &'static str,
    capture: fn(&mut World, u64),
    restore: fn(&mut World, u64),
    truncate_after: fn(&mut World, u64),
    has_tick: fn(&World, u64) -> bool,
    /// Optional serialization support, populated by the networking crate.
    serialize_at: Option<fn(&mut World, u64) -> Option<HashMap<u64, Vec<u8>>>>,
    deserialize_and_apply: Option<fn(&mut World, u64, &HashMap<u64, Vec<u8>>)>,
}

impl TickedComponentRegistry {
    pub fn register<T: TickedComponent>(&mut self) {
        self.register_inner::<T>(None, None);
    }

    /// Register with serialization support. Called by the networking crate.
    pub fn register_with_serialization<T: TickedComponent>(
        &mut self,
        serialize_at: fn(&mut World, u64) -> Option<HashMap<u64, Vec<u8>>>,
        deserialize_and_apply: fn(&mut World, u64, &HashMap<u64, Vec<u8>>),
    ) {
        self.register_inner::<T>(Some(serialize_at), Some(deserialize_and_apply));
    }

    fn register_inner<T: TickedComponent>(
        &mut self,
        serialize_at: Option<fn(&mut World, u64) -> Option<HashMap<u64, Vec<u8>>>>,
        deserialize_and_apply: Option<fn(&mut World, u64, &HashMap<u64, Vec<u8>>)>,
    ) {
        let type_id = TypeId::of::<T>();
        let tname = type_name::<T>();

        if self.type_indices.contains_key(&type_id) {
            panic!("Ticked component type `{tname}` was registered more than once");
        }

        let next_index = u16::try_from(self.entries.len()).unwrap_or_else(|_| {
            panic!(
                "Too many ticked component types registered: maximum is {}",
                u16::MAX
            )
        });

        self.entries.push(RegisteredTickedComponent {
            _type_name: tname,
            capture: capture_component::<T>,
            restore: restore_component::<T>,
            truncate_after: truncate_component::<T>,
            has_tick: has_tick_component::<T>,
            serialize_at,
            deserialize_and_apply,
        });
        self.type_indices.insert(type_id, next_index);
    }

    /// Get the index for a component type.
    pub fn index_of<T: TickedComponent>(&self) -> Option<u16> {
        self.type_indices.get(&TypeId::of::<T>()).copied()
    }

    /// Number of registered component types.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Check if any registered component has captured state at the given tick.
    pub fn has_tick_captured(&self, world: &World, tick: u64) -> bool {
        self.entries.iter().any(|entry| (entry.has_tick)(world, tick))
    }

    /// Capture all registered components at the given tick.
    pub fn capture_all(&self, world: &mut World, tick: u64) {
        for entry in &self.entries {
            (entry.capture)(world, tick);
        }
    }

    /// Restore all registered components from the given tick.
    pub fn restore_all(&self, world: &mut World, tick: u64) {
        for entry in &self.entries {
            (entry.restore)(world, tick);
        }
    }

    /// Truncate all WorldActions history after the given tick.
    pub fn truncate_all_after(&self, world: &mut World, tick: u64) {
        for entry in &self.entries {
            (entry.truncate_after)(world, tick);
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
        for (i, entry) in self.entries.iter().enumerate() {
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
            if let Some(entry) = self.entries.get(*index as usize) {
                if let Some(deserialize_fn) = entry.deserialize_and_apply {
                    deserialize_fn(world, tick, data);
                }
            }
        }
    }
}

/// Extension trait for registering ticked components on the App.
pub trait TickedAppExt {
    /// Register a component for tick-based state tracking.
    fn register_ticked_component<T: TickedComponent>(&mut self) -> &mut Self;
}

impl TickedAppExt for App {
    fn register_ticked_component<T: TickedComponent>(&mut self) -> &mut Self {
        self.init_resource::<TickedComponentRegistry>();
        self.init_resource::<WorldActions<T>>();
        let mut registry = self.world_mut().resource_mut::<TickedComponentRegistry>();
        registry.register::<T>();
        self
    }
}

// --- Type-erased dispatch functions ---

fn capture_component<T: TickedComponent>(world: &mut World, tick: u64) {
    let mut state: HashMap<u64, T> = HashMap::new();

    let mut query = world.query::<(&TickTrackedEntity, &T)>();
    for (net_id, component) in query.iter(world) {
        state.insert(net_id.0, component.clone());
    }

    world
        .resource_mut::<WorldActions<T>>()
        .set_tick(tick, state);
}

fn restore_component<T: TickedComponent>(world: &mut World, tick: u64) {
    let saved = world
        .resource::<WorldActions<T>>()
        .at_tick(tick)
        .cloned();

    let Some(saved) = saved else {
        return;
    };

    let mut query = world.query::<(Entity, &TickTrackedEntity)>();
    let entity_map: Vec<(Entity, u64)> = query
        .iter(world)
        .map(|(entity, net_id)| (entity, net_id.0))
        .collect();

    for (entity, net_id) in &entity_map {
        if let Some(component) = saved.get(net_id) {
            world.entity_mut(*entity).insert(component.clone());
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

fn has_tick_component<T: TickedComponent>(world: &World, tick: u64) -> bool {
    world
        .resource::<WorldActions<T>>()
        .at_tick(tick)
        .is_some()
}
