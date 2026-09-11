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

/// Incoming event: a client has left the session.
///
/// Transport layers trigger this on the host when a client goes — kicked, disconnected, or
/// gone of its own accord. The server forgets everything it held per sender for that uuid: its
/// queued inputs at every tick, its [`InputMargins`](crate::server::InputMargins) entry, its
/// [`NewestInputTick`](crate::server::NewestInputTick) high-water mark. Without this a departed
/// player's last inputs kept being applied to its body until the window pruned them, its margin
/// kept riding in every snapshot, and a peer that rejoined under the same uuid inherited a
/// "newest tick" from its previous life that made its first inputs all read as stale.
#[derive(Event, Clone, Copy, Debug, PartialEq, Eq)]
pub struct PeerLeft(pub u128);

/// Outgoing event: request to send a world snapshot to clients.
///
/// The multiplayer server triggers this after each tick. Transport layers observe it.
#[derive(Event, Clone, Debug)]
pub struct SendNetworkSnapshot(pub WorldSnapshot);

/// Outgoing event: request to send the local player's recent inputs to the server.
///
/// The multiplayer client triggers this each tick. Transport layers observe it.
///
/// Carries the last few ticks of input (ascending tick order) so that a dropped
/// packet self-heals: input for tick T also rides in the packets sent at T+1 and
/// T+2. This makes unreliable delivery safe without retransmits.
#[derive(Event, Clone, Debug)]
pub struct SendNetworkInput<T: TickedInput> {
    /// `(tick, input)` pairs in ascending tick order, newest last.
    pub inputs: Vec<(u64, T)>,
}

/// Serializable wrapper for snapshots sent over the network.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NetworkSnapshotPayload {
    pub snapshot: WorldSnapshot,
}

/// Serializable wrapper for inputs sent over the network.
///
/// Contains redundant recent history (see [`SendNetworkInput`]); receivers apply
/// entries in ascending tick order so the newest entry wins for margin tracking.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NetworkInputPayload<T> {
    /// `(tick, input)` pairs in ascending tick order, newest last.
    pub inputs: Vec<(u64, T)>,
}
