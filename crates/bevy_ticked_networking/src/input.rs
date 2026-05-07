use std::collections::HashMap;

use bevy::prelude::*;
use serde::{Serialize, de::DeserializeOwned};

/// Trait bound for input types that can be sent over the network and replayed during rollback.
pub trait TickedInput:
    Serialize + DeserializeOwned + Clone + Send + Sync + 'static
{
}

impl<T> TickedInput for T where
    T: Serialize + DeserializeOwned + Clone + Send + Sync + 'static
{
}

/// Stores player inputs indexed by tick and player UUID.
///
/// Used by both server (all players' inputs) and client (local player's inputs for replay).
#[derive(Resource)]
pub struct InputQueue<T: TickedInput> {
    /// tick -> (player_uuid -> input)
    pub inputs: HashMap<u64, HashMap<u128, T>>,
}

impl<T: TickedInput> Default for InputQueue<T> {
    fn default() -> Self {
        Self {
            inputs: HashMap::new(),
        }
    }
}

impl<T: TickedInput> InputQueue<T> {
    /// Store an input for a player at a specific tick.
    pub fn insert(&mut self, tick: u64, player_uuid: u128, input: T) {
        self.inputs.entry(tick).or_default().insert(player_uuid, input);
    }

    /// Get a specific player's input at a specific tick.
    pub fn get(&self, tick: u64, player_uuid: u128) -> Option<&T> {
        self.inputs.get(&tick)?.get(&player_uuid)
    }

    /// Get all players' inputs at a specific tick.
    pub fn at_tick(&self, tick: u64) -> Option<&HashMap<u128, T>> {
        self.inputs.get(&tick)
    }

    /// Remove all inputs before a given tick (cleanup old history).
    pub fn prune_before(&mut self, tick: u64) {
        self.inputs.retain(|&t, _| t >= tick);
    }
}
