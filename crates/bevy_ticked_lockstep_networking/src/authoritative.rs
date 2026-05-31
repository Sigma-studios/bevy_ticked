use crate::{
    ActionTracker, AuthoritativeTick, ClientSnapshotState, LastBroadcastTick, LockstepAction,
    LockstepConfig, LockstepLobbyParticipant, PendingClientJoins, StashedAuthoritativeTicks,
    insert_actions_into_tracker, participant_is_required_for_tick,
};
use bevy::prelude::*;
use bevy_ensemble::{
    Host, Lobby, LobbyClient, LobbyClientMessage, LobbyClientPlayerUuid, LobbyMessage,
    LobbyParticipant, LobbyParticipantOf, ReceivedEnsembleMessage,
};
use bevy_ticked::tick::CurrentTick;


pub fn tracker_has_actions_for_player<A>(
    tracker: &ActionTracker<A>,
    tick: u64,
    player_uuid: u128,
) -> bool {
    tracker
        .ticks
        .get(&tick)
        .is_some_and(|players_actions| players_actions.contains_key(&player_uuid))
}

pub fn broadcast_buffered_authoritative_actions_to_loaded_clients<A: LockstepAction>(
    mut commands: Commands,
    current_tick: Res<CurrentTick>,
    config: Res<LockstepConfig>,
    tracker: Res<ActionTracker<A>>,
    mut pending_client_joins: ResMut<PendingClientJoins>,
    lobby_clients: Query<(Entity, &LobbyClientPlayerUuid), With<LobbyClient>>,
    mut messages: MessageReader<ReceivedEnsembleMessage<crate::ClientLoaded>>,
) {
    let end_tick = current_tick.0 + config.host_tick_buffer;
    for loaded_client in messages.read().filter_map(|message| message.sender) {
        let Some((client_entity, _)) = lobby_clients
            .iter()
            .find(|(_, player_uuid)| player_uuid.0 == loaded_client)
        else {
            continue;
        };

        let start_tick = pending_client_joins
            .0
            .get(&loaded_client)
            .map(|snapshot_tick| snapshot_tick.saturating_add(1))
            .unwrap_or_else(|| current_tick.0 + 1);

        for tick in start_tick..=end_tick {
            let Some(players_actions) = tracker.ticks.get(&tick) else {
                continue;
            };

            let message = AuthoritativeTick {
                tick,
                players_actions: players_actions
                    .iter()
                    .map(|(k, v)| (*k, v.clone()))
                    .collect(),
            };
            commands
                .entity(client_entity)
                .trigger(move |entity| LobbyClientMessage::new(entity, message));
        }

        // Client has received the buffered actions; stop preserving old ticks for them
        pending_client_joins.0.remove(&loaded_client);
    }
}

pub fn broadcast_authoritative_actions<A: LockstepAction>(
    mut commands: Commands,
    current_tick: Res<CurrentTick>,
    mut last_broadcast_tick: ResMut<LastBroadcastTick>,
    config: Res<LockstepConfig>,
    tracker: Res<ActionTracker<A>>,
    host_lobby: Option<Single<Entity, (With<Lobby>, With<Host>)>>,
    lobby_clients: Query<(), With<LobbyClient>>,
    participants: Query<(&LobbyParticipant, &LockstepLobbyParticipant, &LobbyParticipantOf)>,
) {
    let Some(host_lobby) = host_lobby else {
        return;
    };

    // No connected clients — nothing to broadcast. Keep last_broadcast_tick
    // current so cleanup doesn't create a gap for future broadcasts.
    if lobby_clients.is_empty() {
        last_broadcast_tick.0 = current_tick.0;
        return;
    }

    for tick in (last_broadcast_tick.0 + 1)..=current_tick.0 {
        let Some(players_actions) = tracker.ticks.get(&tick) else {
            break;
        };

        let mut has_missing_established = false;
        let mut broadcast_actions: Vec<(u128, Vec<A>)> = players_actions
            .iter()
            .map(|(k, v)| (*k, v.clone()))
            .collect();

        for (participant, lockstep_participant, _) in
            participants.iter().filter(|(_, lockstep_participant, pof)| {
                pof.0 == *host_lobby && participant_is_required_for_tick(lockstep_participant, tick)
            })
        {
            if players_actions.contains_key(&participant.player_uuid) {
                continue;
            }
            // Participant is required but missing from the tracker for this tick.
            // If still in their initial buffer window, their actions are
            // implicitly empty.
            if tick <= lockstep_participant.joined_at_tick + config.host_tick_buffer {
                broadcast_actions.push((participant.player_uuid, Vec::new()));
            } else {
                has_missing_established = true;
                break;
            }
        }
        if has_missing_established {
            break;
        }

        let message = AuthoritativeTick {
            tick,
            players_actions: broadcast_actions,
        };
        commands
            .entity(*host_lobby)
            .trigger(move |entity| LobbyMessage::new(entity, message));
        last_broadcast_tick.0 = tick;
    }
}

pub fn receive_authoritative_actions<A: LockstepAction, S: crate::JoinSnapshot>(
    mut messages: MessageReader<ReceivedEnsembleMessage<AuthoritativeTick<A>>>,
    mut tracker: ResMut<ActionTracker<A>>,
    mut stashed_authoritative_ticks: ResMut<StashedAuthoritativeTicks<A>>,
    snapshot_state: Res<ClientSnapshotState<S>>,
    client_lobbies: Query<(), (With<Lobby>, Without<Host>)>,
) {
    if client_lobbies.is_empty() {
        return;
    }

    for message in messages.read() {
        let authoritative_tick = message.message.clone();
        if !snapshot_state.ready {
            stashed_authoritative_ticks.0.push(authoritative_tick);
            continue;
        }

        apply_authoritative_tick(&mut tracker, &authoritative_tick);
    }
}

pub fn replay_stashed_authoritative_actions<A: LockstepAction, S: crate::JoinSnapshot>(
    mut tracker: ResMut<ActionTracker<A>>,
    mut stashed_authoritative_ticks: ResMut<StashedAuthoritativeTicks<A>>,
    snapshot_state: Res<ClientSnapshotState<S>>,
    client_lobbies: Query<(), (With<Lobby>, Without<Host>)>,
) {
    if client_lobbies.is_empty()
        || !snapshot_state.ready
        || stashed_authoritative_ticks.0.is_empty()
    {
        return;
    }
    for authoritative_tick in stashed_authoritative_ticks.0.drain(..) {
        apply_authoritative_tick(&mut tracker, &authoritative_tick);
    }
}

fn apply_authoritative_tick<A: Clone>(
    tracker: &mut ActionTracker<A>,
    authoritative_tick: &AuthoritativeTick<A>,
) {
    for (player_uuid, actions) in &authoritative_tick.players_actions {
        insert_actions_into_tracker(
            tracker,
            authoritative_tick.tick,
            *player_uuid,
            actions.clone(),
        );
    }
}

/// Remove tracker entries for ticks that have already been simulated and broadcast,
/// but preserve any ticks still needed by pending client joins.
pub fn cleanup_old_tracker_entries<A: LockstepAction>(
    mut tracker: ResMut<ActionTracker<A>>,
    current_tick: Res<CurrentTick>,
    pending_client_joins: Res<PendingClientJoins>,
) {
    let min_keep = pending_client_joins
        .0
        .values()
        .copied()
        .min()
        .map(|snapshot_tick| snapshot_tick + 1)
        .unwrap_or(current_tick.0)
        .min(current_tick.0);
    tracker.ticks.retain(|tick, _| *tick >= min_keep);
}
