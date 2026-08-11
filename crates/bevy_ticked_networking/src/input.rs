use std::collections::HashMap;

use bevy::prelude::*;
use bevy_ticked::{
    TickedLoop, TickedSystems,
    tick::{CurrentTick, HistoryBufferTicks},
};
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

/// Create the queue and keep it bounded, exactly once.
///
/// Both [`TickedClientPlugin`](crate::client::TickedClientPlugin) and
/// [`TickedServerPlugin`](crate::server::TickedServerPlugin) call this, and a peer
/// that can host *or* join adds both. The `contains_resource` check is what makes
/// that safe: whichever plugin is built first installs the pruning system, and the
/// second finds the queue already there and does nothing.
pub(crate) fn install_input_queue<T: TickedInput>(app: &mut App) {
    if app.world().contains_resource::<InputQueue<T>>() {
        return;
    }
    app.init_resource::<InputQueue<T>>().add_systems(
        TickedLoop,
        prune_input_queue::<T>.in_set(TickedSystems::PostTick),
    );
}

/// Drop inputs older than the retained history window.
///
/// The queue used to grow for the life of the session on every peer: one outer map
/// entry per tick, plus an entry per player, and nothing ever removed. At 64 Hz
/// that is a quarter of a million nested maps an hour.
///
/// [`HistoryBufferTicks`] is the right window and not merely a convenient one. A
/// rollback never reaches further back than the oldest tick with captured component
/// state — `restore_component` returns early without it — so an input older than
/// that window cannot be replayed even if it were kept.
fn prune_input_queue<T: TickedInput>(
    tick: Res<CurrentTick>,
    buffer: Res<HistoryBufferTicks>,
    mut queue: ResMut<InputQueue<T>>,
) {
    let oldest = tick.0.saturating_sub(buffer.0);
    if oldest > 0 {
        queue.prune_before(oldest);
    }
}
