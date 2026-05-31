use crate::{ClientLoaded, LastBroadcastTick, LockstepConfig, ParticipantJoined};
use bevy::prelude::*;
use bevy_ensemble::{
    Host, Lobby, LobbyClient, LobbyClientMessage, LobbyClientPlayerUuid, LobbyMessage,
    LobbyParticipant, LobbyParticipantOf, ReceivedEnsembleMessage,
};
use bevy_ticked::tick::CurrentTick;
use std::collections::HashMap;

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

pub fn broadcast_participants_to_loaded_clients(
    mut commands: Commands,
    host_lobby: Option<Single<Entity, (With<Lobby>, With<Host>)>>,
    all_participants: Query<(&LobbyParticipant, &LockstepLobbyParticipant, &LobbyParticipantOf)>,
    lobby_clients: Query<(Entity, &LobbyClientPlayerUuid), With<LobbyClient>>,
    mut client_loaded_messages: MessageReader<ReceivedEnsembleMessage<ClientLoaded>>,
) {
    let Some(host_lobby) = host_lobby else {
        return;
    };

    for loaded_client in client_loaded_messages
        .read()
        .filter_map(|message| message.sender)
    {
        let Some((client_entity, _)) = lobby_clients
            .iter()
            .find(|(_, player_uuid)| player_uuid.0 == loaded_client)
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
) {
    let Some(host_lobby) = host_lobby else {
        return;
    };

    for loaded_client in client_loaded_messages
        .read()
        .filter_map(|message| message.sender)
    {
        let Some((participant_entity, _, _, _)) =
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

        let joined_at_tick = current_tick.0 + config.host_tick_buffer + 1;
        commands
            .entity(participant_entity)
            .insert(LockstepLobbyParticipant { joined_at_tick });
    }
}

pub fn broadcast_new_participants_to_existing_clients(
    mut commands: Commands,
    host_lobby: Option<Single<Entity, (With<Lobby>, With<Host>)>>,
    added_participants: Query<
        (&LobbyParticipant, &LockstepLobbyParticipant, &LobbyParticipantOf),
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
) {
    let Some(client_lobby) = client_lobby else {
        return;
    };

    for message in messages.read() {
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

