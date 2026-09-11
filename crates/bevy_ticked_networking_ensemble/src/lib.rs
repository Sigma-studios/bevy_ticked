pub mod handshake;
#[cfg(feature = "overlay")]
mod overlay;
pub mod session;

pub use handshake::{
    HandshakeTimedOut, HandshakeTimeout, LocalSpawnerSlot, RegistryMismatch, RegistryVerified,
    SpawnerSlots, TickedPeerVerified, TickedRegistryHandshake, TickedSessionWelcome,
};
pub use session::{
    TickedEnsembleSessionPlugin, TickedSessionLobby, is_authoritative, is_solo,
    may_spawn_tracked,
};

use std::marker::PhantomData;

use bevy::prelude::*;
use bevy_ensemble::EnsembleSet;
use bevy_ensemble::prelude::*;
use bevy_ensemble::{LobbyClientMessage, LobbyClientPlayerUuid};
use bevy_ticked_networking::{
    diagnostics::ReplayStats,
    input::TickedInput,
    messages::{
        NetworkInputPayload, ReceivedNetworkInput, ReceivedNetworkSnapshot, ReceivedSnapshotAck,
        SendNetworkInput, SendNetworkSnapshot,
    },
    snapshot::decode_packet,
};
use serde::{Deserialize, Serialize};

/// Ensemble message carrying one encoded snapshot packet.
///
/// The bytes are [`encode_packet`](bevy_ticked_networking::snapshot::encode_packet)'s output,
/// carried opaque: the server encodes once per recipient and the bridge does not decode and
/// re-encode on the way out, which is what made the old shape cost an extra postcard pass per
/// broadcast just to count bytes.
#[derive(Message, Clone, Debug, Serialize, Deserialize)]
pub struct EnsembleSnapshotMessage {
    pub bytes: Vec<u8>,
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
        session::install_lobby_tracking(app);
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

/// Decode each received snapshot and hand it to the client, once the registries are known to
/// agree.
///
/// # The gate
///
/// With the session plugin installed, a packet that arrives before [`RegistryVerified`] is
/// dropped here and counted in [`ReplayStats::dropped_before_handshake`]. It is not held: a
/// snapshot is unreliable by construction and the next one is a tick away, whereas a held
/// packet from a peer that turns out to speak a different wire format would be applied the
/// moment the mismatch was found — which is a world built from bytes that mean something else,
/// the failure the handshake exists to prevent. A game that runs the bridge without the session
/// plugin has no handshake and no gate, as before.
///
/// # A packet that does not decode is dropped, never a panic
///
/// The fuzz tests feed this garbage on purpose. Warned once: a peer on a bad link produces
/// these at frame rate and a warning per packet says nothing the first did not.
fn forward_received_snapshots(
    mut messages: MessageReader<ReceivedEnsembleMessage<EnsembleSnapshotMessage>>,
    handshake: Option<Res<handshake::HandshakeInstalled>>,
    verified: Option<Res<RegistryVerified>>,
    mut stats: Option<ResMut<ReplayStats>>,
    mut commands: Commands,
) {
    for msg in messages.read() {
        if handshake.is_some() && verified.is_none() {
            if let Some(stats) = stats.as_mut() {
                stats.dropped_before_handshake += 1;
            }
            continue;
        }
        let Some(packet) = decode_packet(&msg.message.bytes) else {
            warn_once!(
                "a snapshot packet of {} bytes did not decode and was dropped (said once; a \
                 peer on a corrupting link produces these at frame rate)",
                msg.message.bytes.len()
            );
            continue;
        };
        commands.trigger(ReceivedNetworkSnapshot(packet));
    }
}

/// Forward received ensemble input messages to the multiplayer crate's global observer, then
/// the acknowledgement the packet carried.
///
/// The ack goes after the inputs so that a server reading `LastAck` inside an input observer
/// sees the value from before this packet, which is the order the client wrote them in.
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
        if let Some(seq) = msg.message.payload.ack {
            commands.trigger(ReceivedSnapshotAck { sender, seq });
        }
    }
}

// --- Multiplayer -> Ensemble (outgoing) ---

/// Send one encoded snapshot to the client it was built for.
///
/// Addressed, not broadcast: the packet carries that client's own sequence number and margin,
/// and a copy at any other client would be applied as if it were theirs. `recipient: None`
/// means the server had no [`SnapshotRecipientList`](bevy_ticked_networking::server::SnapshotRecipientList)
/// — a bridge without the session plugin — and the one packet goes to the lobby, which on a
/// host fans out to every client, as it did before packets were addressed.
///
/// A recipient with no `LobbyClient` any more is a client that left between the tick and this
/// observer; its packet has nowhere to go and is dropped.
fn forward_outgoing_snapshots(
    trigger: On<SendNetworkSnapshot>,
    lobby: Option<Res<TickedSessionLobby>>,
    clients: Query<(Entity, &LobbyClientPlayerUuid), With<LobbyClient>>,
    mut commands: Commands,
) {
    let event = trigger.event();
    let message = EnsembleSnapshotMessage {
        bytes: event.bytes.clone(),
    };
    match event.recipient {
        Some(uuid) => {
            let Some((client, _)) = clients.iter().find(|(_, client)| client.0 == uuid) else {
                debug!("a snapshot for {uuid:#x} has no client to go to; it left");
                return;
            };
            commands.entity(client).trigger(move |entity| LobbyClientMessage {
                entity,
                message,
                send_mode: SendMode::Unreliable,
            });
        }
        None => {
            let Some(lobby) = lobby else { return };
            commands.entity(lobby.0).trigger(move |entity| LobbyMessage {
                entity,
                message,
                send_mode: SendMode::Unreliable,
            });
        }
    }
}

/// Forward outgoing input events to ensemble as lobby messages.
fn forward_outgoing_inputs<T: TickedInput + Serialize + for<'de> Deserialize<'de>>(
    trigger: On<SendNetworkInput<T>>,
    lobby: Option<Res<TickedSessionLobby>>,
    mut commands: Commands,
) {
    let Some(lobby) = lobby else { return };
    let message = EnsembleInputMessage {
        payload: NetworkInputPayload {
            inputs: trigger.event().inputs.clone(),
            ack: trigger.event().ack,
        },
    };
    // Unreliable: a lost packet is cheaper than head-of-line blocking the
    // inputs behind it, and the redundant history in each payload means a
    // drop only matters if INPUT_REDUNDANCY consecutive packets are lost.
    commands.entity(lobby.0).trigger(move |entity| LobbyMessage {
        entity,
        message,
        send_mode: SendMode::Unreliable,
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy_ticked_networking::snapshot::{FullBody, SnapshotBody, SnapshotPacket, encode_packet};

    #[derive(Clone, Serialize, serde::Deserialize)]
    struct Input;

    #[derive(Resource, Default)]
    struct Arrivals(Vec<(u32, u64)>);

    #[derive(Resource, Default)]
    struct Frame(u32);

    fn packet_at(tick: u64) -> Vec<u8> {
        encode_packet(&SnapshotPacket {
            seq: 1,
            tick,
            your_margin: 0,
            body: SnapshotBody::Full(FullBody::default()),
        })
    }

    /// Stands in for a backend's receive system: exclusive, in `ReceivePackets`,
    /// and it writes the snapshot message directly, as `decode_ensemble_packet` does.
    fn fake_transport(world: &mut World) {
        let frame = world.resource::<Frame>().0;
        world.write_message(ReceivedEnsembleMessage {
            sender: Some(1),
            message: EnsembleSnapshotMessage {
                bytes: packet_at(u64::from(frame)),
            },
            received_at: bevy_ensemble::Instant::now(),
        });
    }

    fn bridged_app() -> App {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins)
            .add_plugins(EnsemblePlugin)
            .init_resource::<Arrivals>()
            .init_resource::<Frame>()
            .add_plugins(TickedNetworkingEnsemblePlugin::<Input>::new())
            .add_observer(
                |snapshot: On<ReceivedNetworkSnapshot>,
                 frame: Res<Frame>,
                 mut arrivals: ResMut<Arrivals>| {
                    arrivals.0.push((frame.0, snapshot.event().0.tick));
                },
            )
            .add_systems(Last, |mut frame: ResMut<Frame>| frame.0 += 1);
        app
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
        let mut app = bridged_app();
        app.add_systems(PreUpdate, (busy, busy, busy))
            .add_systems(
                PreUpdate,
                fake_transport.in_set(EnsembleSet::ReceivePackets),
            );

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

    /// Bytes that are not a packet are dropped where they arrive. A panic here is a remote
    /// crash for every client the sender can reach.
    #[test]
    fn a_snapshot_that_does_not_decode_is_dropped() {
        let mut app = bridged_app();
        app.world_mut().write_message(ReceivedEnsembleMessage {
            sender: Some(1),
            message: EnsembleSnapshotMessage {
                bytes: vec![0xff; 40],
            },
            received_at: bevy_ensemble::Instant::now(),
        });
        app.world_mut().write_message(ReceivedEnsembleMessage {
            sender: Some(1),
            message: EnsembleSnapshotMessage { bytes: Vec::new() },
            received_at: bevy_ensemble::Instant::now(),
        });
        app.update();
        assert!(app.world().resource::<Arrivals>().0.is_empty());
    }

    /// With the handshake installed, nothing reaches the client until the registries have
    /// been compared; the drop is counted where a test can read it.
    #[test]
    fn a_snapshot_before_verification_is_dropped_and_counted() {
        let mut app = bridged_app();
        app.init_resource::<ReplayStats>()
            .insert_resource(handshake::HandshakeInstalled);
        app.world_mut().write_message(ReceivedEnsembleMessage {
            sender: Some(1),
            message: EnsembleSnapshotMessage {
                bytes: packet_at(3),
            },
            received_at: bevy_ensemble::Instant::now(),
        });
        app.update();
        assert!(app.world().resource::<Arrivals>().0.is_empty());
        assert_eq!(
            app.world().resource::<ReplayStats>().dropped_before_handshake,
            1
        );

        app.insert_resource(RegistryVerified);
        app.world_mut().write_message(ReceivedEnsembleMessage {
            sender: Some(1),
            message: EnsembleSnapshotMessage {
                bytes: packet_at(4),
            },
            received_at: bevy_ensemble::Instant::now(),
        });
        app.update();
        assert_eq!(app.world().resource::<Arrivals>().0.len(), 1);
    }

    /// The ack rides on the input packet and is handed to the server after the inputs.
    #[test]
    fn an_input_packets_ack_is_forwarded() {
        #[derive(Resource, Default)]
        struct Acks(Vec<ReceivedSnapshotAck>);
        let mut app = App::new();
        app.add_plugins(MinimalPlugins)
            .add_plugins(EnsemblePlugin)
            .init_resource::<Acks>()
            .add_plugins(TickedNetworkingEnsemblePlugin::<Input>::new())
            .add_observer(|ack: On<ReceivedSnapshotAck>, mut acks: ResMut<Acks>| {
                acks.0.push(*ack.event());
            });
        app.world_mut().write_message(ReceivedEnsembleMessage {
            sender: Some(7),
            message: EnsembleInputMessage {
                payload: NetworkInputPayload {
                    inputs: vec![(1, Input)],
                    ack: Some(12),
                },
            },
            received_at: bevy_ensemble::Instant::now(),
        });
        app.update();
        assert_eq!(
            app.world().resource::<Acks>().0,
            vec![ReceivedSnapshotAck { sender: 7, seq: 12 }]
        );
    }
}
