use bevy::prelude::*;
use serde::{Deserialize, Serialize};

use crate::snapshot::WorldSnapshot;

use crate::input::TickedInput;

/// Incoming event: a world snapshot received from the server.
///
/// Transport layers trigger this via `commands.trigger()` when a snapshot arrives.
#[derive(Event, Clone, Debug)]
pub struct ReceivedNetworkSnapshot(pub WorldSnapshot);

/// Incoming event: a player's input received from the network.
///
/// Transport layers trigger this via `commands.trigger()` when an input arrives.
#[derive(Event, Clone, Debug)]
pub struct ReceivedNetworkInput<T: TickedInput> {
    pub sender: u128,
    pub tick: u64,
    pub input: T,
}

/// Outgoing event: request to send a world snapshot to clients.
///
/// The multiplayer server triggers this after each tick. Transport layers observe it.
#[derive(Event, Clone, Debug)]
pub struct SendNetworkSnapshot(pub WorldSnapshot);

/// Outgoing event: request to send the local player's input to the server.
///
/// The multiplayer client triggers this each tick. Transport layers observe it.
#[derive(Event, Clone, Debug)]
pub struct SendNetworkInput<T: TickedInput> {
    pub tick: u64,
    pub input: T,
}

/// Serializable wrapper for snapshots sent over the network.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NetworkSnapshotPayload {
    pub snapshot: WorldSnapshot,
}

/// Serializable wrapper for inputs sent over the network.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NetworkInputPayload<T> {
    pub tick: u64,
    pub input: T,
}
