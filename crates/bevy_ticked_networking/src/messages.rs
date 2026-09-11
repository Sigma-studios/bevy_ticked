use bevy::prelude::*;
use serde::{Deserialize, Serialize};

use crate::input::TickedInput;
use crate::snapshot::SnapshotPacket;

/// Incoming event: a snapshot packet received from the server.
///
/// Transport layers trigger this via `commands.trigger()` when a snapshot arrives, after
/// decoding it with [`decode_packet`](crate::snapshot::decode_packet).
#[derive(Event, Clone, Debug)]
pub struct ReceivedNetworkSnapshot(pub SnapshotPacket);

/// Incoming event: a client acknowledged the newest snapshot it has applied.
///
/// Rides on every input packet as [`NetworkInputPayload::ack`]; transport layers trigger this
/// once per input packet that carries one. The server keeps the newest per client
/// ([`LastAck`](crate::server::LastAck)), which is what a delta is built against.
#[derive(Event, Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReceivedSnapshotAck {
    pub sender: u128,
    pub seq: u32,
    /// The client could not rebuild a delta (its baseline was gone) and wants a full body.
    pub nack_full: bool,
}

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

/// Outgoing event: one encoded snapshot packet for one client.
///
/// The server triggers one per recipient after each tick. Transport layers observe it and send
/// `bytes` to `recipient` unreliably. `recipient` is `None` only when no
/// [`SnapshotRecipientList`](crate::server::SnapshotRecipientList) was installed, which means a
/// transport that has not said who is listening: send to everyone.
#[derive(Event, Clone, Debug)]
pub struct SendNetworkSnapshot {
    pub recipient: Option<u128>,
    pub bytes: Vec<u8>,
}

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
    /// `seq` of the newest snapshot this client has applied.
    pub ack: Option<u32>,
    /// Ask for a full body next: a delta arrived against a baseline this client no longer has.
    pub nack_full: bool,
}

/// Serializable wrapper for inputs sent over the network.
///
/// Contains redundant recent history (see [`SendNetworkInput`]); receivers apply
/// entries in ascending tick order so the newest entry wins for margin tracking.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NetworkInputPayload<T> {
    /// `(tick, input)` pairs in ascending tick order, newest last.
    pub inputs: Vec<(u64, T)>,
    /// `seq` of the newest snapshot the sender has applied. See [`ReceivedSnapshotAck`].
    #[serde(default)]
    pub ack: Option<u32>,
    /// See [`SendNetworkInput::nack_full`].
    #[serde(default)]
    pub nack_full: bool,
}
