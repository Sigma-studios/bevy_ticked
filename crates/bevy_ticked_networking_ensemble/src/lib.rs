pub mod handshake;
pub mod session;

pub use handshake::{RegistryMismatch, TickedRegistryHandshake};
pub use session::{
    is_authoritative, is_solo, may_spawn_tracked, TickedEnsembleSessionPlugin,
};

use std::marker::PhantomData;

use bevy::prelude::*;
use bevy_ensemble::prelude::*;
use bevy_ticked_networking::{
    input::TickedInput,
    messages::{
        NetworkInputPayload, NetworkSnapshotPayload, ReceivedNetworkInput, ReceivedNetworkSnapshot,
        SendNetworkInput, SendNetworkSnapshot,
    },
};
use serde::{Deserialize, Serialize};

/// Ensemble message type wrapping a network snapshot.
#[derive(Message, Clone, Debug, Serialize, Deserialize)]
pub struct EnsembleSnapshotMessage {
    pub payload: NetworkSnapshotPayload,
}

/// Ensemble message type wrapping a player's input.
#[derive(Message, Clone, Debug, Serialize, Deserialize)]
#[serde(bound(deserialize = "T: serde::de::DeserializeOwned"))]
pub struct EnsembleInputMessage<T: TickedInput> {
    pub payload: NetworkInputPayload<T>,
}

/// Plugin that bridges bevy_ticked_networking's global observers with bevy_ensemble messaging.
///
/// Registers the snapshot and input message types with ensemble, and adds systems
/// that forward between `ReceivedEnsembleMessage<T>` / `LobbyMessage<T>` and
/// the multiplayer crate's global observer events.
pub struct TickedNetworkingEnsemblePlugin<T: TickedInput + Serialize + for<'de> Deserialize<'de>> {
    _phantom: PhantomData<T>,
}

impl<T: TickedInput + Serialize + for<'de> Deserialize<'de>> TickedNetworkingEnsemblePlugin<T> {
    pub fn new() -> Self {
        Self {
            _phantom: PhantomData,
        }
    }
}

impl<T: TickedInput + Serialize + for<'de> Deserialize<'de>> Default
    for TickedNetworkingEnsemblePlugin<T>
{
    fn default() -> Self {
        Self::new()
    }
}

impl<T: TickedInput + Serialize + for<'de> Deserialize<'de>> Plugin
    for TickedNetworkingEnsemblePlugin<T>
{
    fn build(&self, app: &mut App) {
        app.register_ensemble_message_type::<EnsembleSnapshotMessage>()
            .register_ensemble_message_type::<EnsembleInputMessage<T>>()
            .add_systems(
                PreUpdate,
                (forward_received_snapshots, forward_received_inputs::<T>),
            )
            .add_observer(forward_outgoing_snapshots)
            .add_observer(forward_outgoing_inputs::<T>);
    }
}

// --- Ensemble -> Multiplayer (incoming) ---

/// Forward received ensemble snapshot messages to the multiplayer crate's global observer.
fn forward_received_snapshots(
    mut messages: MessageReader<ReceivedEnsembleMessage<EnsembleSnapshotMessage>>,
    mut commands: Commands,
) {
    for msg in messages.read() {
        commands.trigger(ReceivedNetworkSnapshot(
            msg.message.payload.snapshot.clone(),
        ));
    }
}

/// Forward received ensemble input messages to the multiplayer crate's global observer.
fn forward_received_inputs<T: TickedInput + Serialize + for<'de> Deserialize<'de>>(
    mut messages: MessageReader<ReceivedEnsembleMessage<EnsembleInputMessage<T>>>,
    mut commands: Commands,
) {
    for msg in messages.read() {
        let Some(sender) = msg.sender else {
            warn!("Received network input with no sender, skipping");
            continue;
        };
        // Apply in ascending tick order so the newest entry is the last to
        // update the server's InputMargins.
        let mut entries = msg.message.payload.inputs.clone();
        entries.sort_by_key(|(tick, _)| *tick);
        for (tick, input) in entries {
            commands.trigger(ReceivedNetworkInput {
                sender,
                tick,
                input,
            });
        }
    }
}

// --- Multiplayer -> Ensemble (outgoing) ---

/// Forward outgoing snapshot events to ensemble as lobby messages.
fn forward_outgoing_snapshots(
    trigger: On<SendNetworkSnapshot>,
    lobby: Option<Single<Entity, With<Lobby>>>,
    mut commands: Commands,
) {
    let Some(lobby) = lobby else { return };
    let lobby_entity = *lobby;
    let message = EnsembleSnapshotMessage {
        payload: NetworkSnapshotPayload {
            snapshot: trigger.event().0.clone(),
        },
    };
    commands
        .entity(lobby_entity)
        .trigger(move |entity| LobbyMessage {
            entity,
            message,
            send_mode: SendMode::Unreliable,
        });
}

/// Forward outgoing input events to ensemble as lobby messages.
fn forward_outgoing_inputs<T: TickedInput + Serialize + for<'de> Deserialize<'de>>(
    trigger: On<SendNetworkInput<T>>,
    lobby: Option<Single<Entity, With<Lobby>>>,
    mut commands: Commands,
) {
    let Some(lobby) = lobby else { return };
    let lobby_entity = *lobby;
    let message = EnsembleInputMessage {
        payload: NetworkInputPayload {
            inputs: trigger.event().inputs.clone(),
        },
    };
    // Unreliable: a lost packet is cheaper than head-of-line blocking the
    // inputs behind it, and the redundant history in each payload means a
    // drop only matters if INPUT_REDUNDANCY consecutive packets are lost.
    commands
        .entity(lobby_entity)
        .trigger(move |entity| LobbyMessage {
            entity,
            message,
            send_mode: SendMode::Unreliable,
        });
}
