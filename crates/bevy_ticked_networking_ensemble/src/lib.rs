pub mod handshake;
#[cfg(feature = "overlay")]
mod overlay;
pub mod session;

pub use handshake::{RegistryMismatch, TickedRegistryHandshake};
pub use session::{
    is_authoritative, is_solo, may_spawn_tracked, TickedEnsembleSessionPlugin,
};

use std::marker::PhantomData;

use bevy::prelude::*;
use bevy_ensemble::EnsembleSet;
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
        #[cfg(feature = "overlay")]
        app.add_plugins(overlay::TickedOverlayPlugin);
        // One snapshot type and one input type per app, so the names are fixed. A snapshot is
        // the authority's word: a client takes it from its host and nobody else.
        app.register_ensemble_message_type_with::<EnsembleSnapshotMessage>(
            "bevy_ticked/Snapshot",
            bevy_ensemble::MessageAuthority::HostOnly,
        )
        .register_ensemble_message_type::<EnsembleInputMessage<T>>("bevy_ticked/Input")
            // After the transport has drained its socket, and not merely in the same
            // schedule. These read `Messages` the backend writes from an exclusive
            // system, and the multi-threaded executor puts an exclusive system
            // behind every parallel system that is already ready -- so with no
            // ordering these ran first, and every packet was read the frame after
            // it arrived. A frame on the input path and a frame on the snapshot
            // path, on native, on every frame.
            .add_systems(
                PreUpdate,
                (forward_received_snapshots, forward_received_inputs::<T>)
                    .after(EnsembleSet::ReceivePackets),
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
    stats: Option<ResMut<bevy_ticked_networking::diagnostics::SnapshotStats>>,
    recipients: Option<Res<bevy_ticked_networking::server::SnapshotRecipients>>,
) {
    let Some(lobby) = lobby else { return };
    let lobby_entity = *lobby;
    let message = EnsembleSnapshotMessage {
        payload: NetworkSnapshotPayload {
            snapshot: trigger.event().0.clone(),
        },
    };
    // The size the wire will carry, per recipient. One extra encode per broadcast; the
    // transport is about to do the same one, and a game that wants this number gone can
    // read `SnapshotStats.sent` instead.
    if let Some(mut stats) = stats
        && let Ok(bytes) = postcard::to_allocvec(&message).map(|v| v.len())
    {
        let recipients = recipients.map_or(1, |r| r.0.max(1));
        for _ in 0..recipients {
            stats.record_bytes(bytes);
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use bevy_ticked_networking::snapshot::WorldSnapshot;

    #[derive(Clone, Serialize, serde::Deserialize)]
    struct Input;

    #[derive(Resource, Default)]
    struct Arrivals(Vec<(u32, u64)>);

    #[derive(Resource, Default)]
    struct Frame(u32);

    /// Stands in for a backend's receive system: exclusive, in `ReceivePackets`,
    /// and it writes the snapshot message directly, as `decode_ensemble_packet` does.
    fn fake_transport(world: &mut World) {
        let frame = world.resource::<Frame>().0;
        world.write_message(ReceivedEnsembleMessage {
            sender: Some(1),
            message: EnsembleSnapshotMessage {
                payload: NetworkSnapshotPayload {
                    snapshot: WorldSnapshot {
                        tick: u64::from(frame),
                        components: Default::default(),
                        resources: Default::default(),
                        input_margins: Default::default(),
                    },
                },
            },
            received_at: bevy_ensemble::Instant::now(),
        });
    }

    /// A packet has to reach the tick loop the frame it comes off the socket.
    ///
    /// The executor is part of what is under test: a handful of unrelated parallel
    /// systems is what displaces an exclusive one, and without them the schedule
    /// happens to run in insertion order and the bug does not show.
    #[test]
    fn a_packet_is_forwarded_the_frame_it_arrives() {
        fn busy(mut acc: Local<u64>) {
            for i in 0..10_000u64 {
                *acc = acc.wrapping_add(i);
            }
        }
        let mut app = App::new();
        app.add_plugins(MinimalPlugins)
            .add_plugins(EnsemblePlugin)
            .init_resource::<Arrivals>()
            .init_resource::<Frame>()
            .add_systems(PreUpdate, (busy, busy, busy))
            .add_systems(
                PreUpdate,
                fake_transport.in_set(EnsembleSet::ReceivePackets),
            )
            .add_plugins(TickedNetworkingEnsemblePlugin::<Input>::new())
            .add_observer(
                |snapshot: On<ReceivedNetworkSnapshot>,
                 frame: Res<Frame>,
                 mut arrivals: ResMut<Arrivals>| {
                    arrivals.0.push((frame.0, snapshot.event().0.tick));
                },
            )
            .add_systems(Last, |mut frame: ResMut<Frame>| frame.0 += 1);

        for _ in 0..8 {
            app.update();
        }

        let arrivals = &app.world().resource::<Arrivals>().0;
        assert!(!arrivals.is_empty(), "nothing was forwarded at all");
        let late: Vec<_> = arrivals
            .iter()
            .filter(|(frame, tick)| u64::from(*frame) != *tick)
            .collect();
        assert!(
            late.is_empty(),
            "snapshots forwarded a frame after they arrived: {late:?}"
        );
    }
}
