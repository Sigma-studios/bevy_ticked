use std::collections::{BTreeMap, BTreeSet};

use bevy::prelude::*;
use bevy_ticked::{
    TickedLoop, TickedSystems,
    tick::{CurrentTick, HistoryBufferTicks},
};
use serde::{Serialize, de::DeserializeOwned};

/// Trait bound for input types that can be sent over the network and replayed during rollback.
pub trait TickedInput: Serialize + DeserializeOwned + Clone + Send + Sync + 'static {}

impl<T> TickedInput for T where T: Serialize + DeserializeOwned + Clone + Send + Sync + 'static {}

/// The furthest ahead of the host's tick an input may be stamped and still be accepted.
///
/// **Must equal the client's tick-buffer ceiling** (`ClientTickBuffer::MAX_TICKS`): a client can
/// never legitimately lead by more than its buffer allows, so an input further ahead than that
/// is either a bug or an attack, and the host refuses it rather than file it where nothing will
/// read it for a minute. See [`collect_network_inputs`](crate::server) for the window this is
/// the upper edge of; the lower edge is [`HistoryBufferTicks`].
///
/// Defined here rather than next to the buffer so that the server does not depend on the client
/// module for a number, and so that the two cannot drift apart without a reader noticing that
/// the client's constant is spelled in terms of this one.
pub const MAX_INPUT_LEAD_TICKS: u64 = 64;

/// Stores player inputs indexed by tick and player UUID.
///
/// Used by both server (all players' inputs) and client (local player's inputs for replay).
///
/// # Ordered on purpose
///
/// Both levels are `BTreeMap`s, and the inner one is what [`at_tick`](Self::at_tick) hands to a
/// simulation. A `HashMap` there iterated in a different order on every peer — `RandomState` is
/// seeded per process — so a simulation that folded over "every player's input this tick" (a
/// pushable crate resolving which player shoved it first, a pickup awarded to whoever pressed
/// use) produced a different world on the host and on each client, and the divergence read as
/// a rollback storm nobody could attribute. In uuid order, the fold is the same everywhere.
#[derive(Resource)]
pub struct InputQueue<T: TickedInput> {
    /// tick -> (player_uuid -> input)
    pub inputs: BTreeMap<u64, BTreeMap<u128, T>>,
}

impl<T: TickedInput> Default for InputQueue<T> {
    fn default() -> Self {
        Self {
            inputs: BTreeMap::new(),
        }
    }
}

impl<T: TickedInput> InputQueue<T> {
    /// Store an input for a player at a specific tick.
    pub fn insert(&mut self, tick: u64, player_uuid: u128, input: T) {
        self.inputs
            .entry(tick)
            .or_default()
            .insert(player_uuid, input);
    }

    /// Get a specific player's input at a specific tick.
    pub fn get(&self, tick: u64, player_uuid: u128) -> Option<&T> {
        self.inputs.get(&tick)?.get(&player_uuid)
    }

    /// Get all players' inputs at a specific tick, in ascending uuid order.
    pub fn at_tick(&self, tick: u64) -> Option<&BTreeMap<u128, T>> {
        self.inputs.get(&tick)
    }

    /// Every player with at least one input in the queue, each once, in ascending uuid order.
    pub fn players(&self) -> impl Iterator<Item = u128> {
        self.inputs
            .values()
            .flat_map(|players| players.keys().copied())
            .collect::<BTreeSet<u128>>()
            .into_iter()
    }

    /// Forget every input `player_uuid` has in the queue, at every tick.
    ///
    /// A tick left with no inputs is dropped with it, so a departed player does not leave a
    /// trail of empty ticks behind for the whole window.
    pub fn remove_player(&mut self, player_uuid: u128) {
        self.inputs.retain(|_, players| {
            players.remove(&player_uuid);
            !players.is_empty()
        });
    }

    /// Remove all inputs before a given tick (cleanup old history).
    pub fn prune_before(&mut self, tick: u64) {
        self.inputs = self.inputs.split_off(&tick);
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
