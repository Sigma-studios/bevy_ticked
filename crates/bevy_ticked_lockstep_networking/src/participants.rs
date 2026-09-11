use crate::{ClientAccepted, ClientLoaded, LastBroadcastTick, LockstepConfig, ParticipantJoined};
use bevy::prelude::*;
use bevy_ensemble::{
    Host, Lobby, LobbyClient, LobbyClientMessage, LobbyClientPlayerUuid, LobbyMessage,
    LobbyParticipant, LobbyParticipantOf, ReceivedEnsembleMessage,
};
use bevy_ticked::tick::CurrentTick;
use std::collections::{HashMap, HashSet};

#[derive(Component, Clone, Copy, Debug, PartialEq, Eq)]
pub struct LockstepLobbyParticipant {
    pub joined_at_tick: u64,
}

#[derive(Resource, Default)]
pub struct PendingLockstepParticipantJoins(pub HashMap<u128, u64>);

pub fn add_host_participant(
    mut commands: Commands,
    current_tick: Res<CurrentTick>,
    mut last_broadcast_tick: ResMut<LastBroadcastTick>,
    added_participants: Query<
        (Entity, &LobbyParticipant, &LobbyParticipantOf),
        Added<LobbyParticipant>,
    >,
    host_lobbies: Query<Entity, (With<Lobby>, With<Host>)>,
) {
    let Some(host_lobby) = host_lobbies.iter().next() else {
        return;
    };

    for (participant_entity, participant, participant_of) in added_participants.iter() {
        if !participant.is_host || participant_of.0 != host_lobby {
            continue;
        }

        last_broadcast_tick.0 = current_tick.0;
        commands
            .entity(participant_entity)
            .insert(LockstepLobbyParticipant {
                joined_at_tick: current_tick.0,
            });
    }
}

/// Host: send a client the roster the moment it is accepted, so it knows whose actions to expect.
pub fn broadcast_participants_to_loaded_clients(
    mut commands: Commands,
    host_lobby: Option<Single<Entity, (With<Lobby>, With<Host>)>>,
    all_participants: Query<(
        &LobbyParticipant,
        &LockstepLobbyParticipant,
        &LobbyParticipantOf,
    )>,
    lobby_clients: Query<(Entity, &LobbyClientPlayerUuid), With<LobbyClient>>,
    mut accepted: MessageReader<ClientAccepted>,
) {
    let Some(host_lobby) = host_lobby else {
        return;
    };

    for accepted in accepted.read() {
        let Some((client_entity, _)) = lobby_clients
            .iter()
            .find(|(_, player_uuid)| player_uuid.0 == accepted.player_uuid)
        else {
            continue;
        };

        for (participant, lockstep_participant, participant_of) in all_participants.iter() {
            if participant_of.0 != *host_lobby {
                continue;
            }
            let message = ParticipantJoined {
                player_uuid: participant.player_uuid,
                joined_at_tick: lockstep_participant.joined_at_tick,
            };
            commands
                .entity(client_entity)
                .trigger(move |entity| LobbyClientMessage::new(entity, message));
        }
    }
}

/// Host: a loaded client becomes a participant, from a tick far enough ahead that its first
/// scheduled actions can be there in time.
///
/// # The window is sized from the larger buffer
///
/// A client schedules `client_tick_buffer` ticks ahead of its own clock, which trails the host's.
/// The host requires the client's actions from `joined_at_tick + host_tick_buffer + 1`, and the
/// window between now and then used to be sized from the host's buffer alone. A client whose
/// buffer was the larger — the adaptive tuner grows it on a bad link, and a player who joined a
/// LAN host after a satellite session carried it in — had its first scheduled tick land after
/// the first required one, and the host waited on the gap for ever. The joiner reports its
/// buffer in [`ClientLoaded`]; the window is the larger of the two.
///
/// # Once
///
/// A `ClientLoaded` from a uuid that is already a participant is ignored. Re-running this for it
/// moved its `joined_at_tick` forward, which reopened the grace window in which its missing
/// actions count as empty — so a client could keep its window open, and its actions optional,
/// for as long as it kept saying it had loaded. Accepting exactly once also gates the roster
/// and the catch-up, which read [`ClientAccepted`] rather than the wire.
pub fn activate_loaded_client_participants(
    mut commands: Commands,
    current_tick: Res<CurrentTick>,
    config: Res<LockstepConfig>,
    host_lobby: Option<Single<Entity, (With<Lobby>, With<Host>)>>,
    participants: Query<(
        Entity,
        &LobbyParticipant,
        Option<&LockstepLobbyParticipant>,
        &LobbyParticipantOf,
    )>,
    mut client_loaded_messages: MessageReader<ReceivedEnsembleMessage<ClientLoaded>>,
    mut accepted: MessageWriter<ClientAccepted>,
) {
    let Some(host_lobby) = host_lobby else {
        return;
    };

    let mut accepted_this_frame = HashSet::new();
    for message in client_loaded_messages.read() {
        let Some(loaded_client) = message.sender else {
            continue;
        };
        let Some((participant_entity, _, lockstep_participant, _)) =
            participants
                .iter()
                .find(|(_, participant, _, participant_of)| {
                    participant_of.0 == *host_lobby && participant.player_uuid == loaded_client
                })
        else {
            warn!(
                "Missing base multiplayer participant for loaded player {} in host lobby {:?}",
                loaded_client, *host_lobby
            );
            continue;
        };
        if lockstep_participant.is_some() || !accepted_this_frame.insert(loaded_client) {
            debug!("ignoring a repeated ClientLoaded from {loaded_client}, already a participant");
            continue;
        }

        let buffer = config.host_tick_buffer.max(message.message.buffer);
        let joined_at_tick = current_tick.0 + 1 + buffer;
        commands
            .entity(participant_entity)
            .insert(LockstepLobbyParticipant { joined_at_tick });
        accepted.write(ClientAccepted {
            player_uuid: loaded_client,
        });
    }
}

pub fn broadcast_new_participants_to_existing_clients(
    mut commands: Commands,
    host_lobby: Option<Single<Entity, (With<Lobby>, With<Host>)>>,
    added_participants: Query<
        (
            &LobbyParticipant,
            &LockstepLobbyParticipant,
            &LobbyParticipantOf,
        ),
        Added<LockstepLobbyParticipant>,
    >,
) {
    let Some(host_lobby) = host_lobby else {
        return;
    };

    for (participant, lockstep_participant, participant_of) in added_participants.iter() {
        if participant_of.0 != *host_lobby {
            continue;
        }
        let message = ParticipantJoined {
            player_uuid: participant.player_uuid,
            joined_at_tick: lockstep_participant.joined_at_tick,
        };
        commands
            .entity(*host_lobby)
            .trigger(move |entity| LobbyMessage::new(entity, message));
    }
}

/// Client: apply the roster the host sent.
///
/// Runs in `PreUpdate`, after the packets are drained and before the tick loop, so a
/// participant is on the roster before the first tick of the frame it arrived in. A game that
/// spawns a player *inside the tick* at `joined_at_tick` — which is the only way every peer
/// spawns it on the same tick — needs the roster to precede that tick on every peer, and a
/// roster applied in `Update` reached the tick loop a frame late.
pub fn apply_received_participants(
    mut commands: Commands,
    mut messages: MessageReader<ReceivedEnsembleMessage<ParticipantJoined>>,
    client_lobby: Option<Single<Entity, (With<Lobby>, Without<Host>)>>,
    participants: Query<(
        Entity,
        &LobbyParticipant,
        Option<&LockstepLobbyParticipant>,
        &LobbyParticipantOf,
    )>,
    mut pending_joins: ResMut<PendingLockstepParticipantJoins>,
    current_tick: Res<CurrentTick>,
    mut roster: ResMut<crate::LockstepRoster>,
    mut changes: bevy_ticked::events::TickedEventWriter<crate::RosterChange>,
) {
    let Some(client_lobby) = client_lobby else {
        return;
    };

    for message in messages.read() {
        // A participant whose agreed tick this peer has already simulated joined before this
        // peer's snapshot: the tick that carried the join is not coming, so the roster is
        // seeded here. One whose tick is ahead is added by that tick, on every peer alike.
        if message.message.joined_at_tick <= current_tick.0
            && roster.0.insert(message.message.player_uuid)
        {
            changes.write(
                current_tick.0,
                crate::RosterChange::Joined(message.message.player_uuid),
            );
        }
        if let Some((participant_entity, _, _, _)) =
            participants
                .iter()
                .find(|(_, participant, _, participant_of)| {
                    participant_of.0 == *client_lobby
                        && participant.player_uuid == message.message.player_uuid
                })
        {
            commands
                .entity(participant_entity)
                .insert(LockstepLobbyParticipant {
                    joined_at_tick: message.message.joined_at_tick,
                });
            continue;
        }

        pending_joins
            .0
            .insert(message.message.player_uuid, message.message.joined_at_tick);
    }
}

pub fn apply_pending_lockstep_participants(
    mut commands: Commands,
    client_lobby: Option<Single<Entity, (With<Lobby>, Without<Host>)>>,
    participants: Query<(
        Entity,
        &LobbyParticipant,
        Option<&LockstepLobbyParticipant>,
        &LobbyParticipantOf,
    )>,
    mut pending_joins: ResMut<PendingLockstepParticipantJoins>,
) {
    let Some(client_lobby) = client_lobby else {
        pending_joins.0.clear();
        return;
    };

    for (participant_entity, participant, lockstep_participant, participant_of) in
        participants.iter()
    {
        if participant_of.0 != *client_lobby || lockstep_participant.is_some() {
            continue;
        }

        let Some(joined_at_tick) = pending_joins.0.remove(&participant.player_uuid) else {
            continue;
        };

        commands
            .entity(participant_entity)
            .insert(LockstepLobbyParticipant { joined_at_tick });
    }
}

pub fn participant_is_required_for_tick(participant: &LockstepLobbyParticipant, tick: u64) -> bool {
    tick >= participant.joined_at_tick
}
