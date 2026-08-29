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

/// Catch a newly-loaded client up on the ticks it missed between its snapshot and now.
///
/// Only ticks the host has actually *simulated* are sent, because only those are complete. The
/// host advances a tick once every established participant's actions for it are in, so
/// `current_tick` is the newest tick guaranteed whole; anything past it is still being filled in.
///
/// This used to send up to `current_tick + host_tick_buffer`, handing the joining client tick
/// after tick that looked authoritative but was missing whichever peers had not reported in yet.
/// The client simulated those half-populated ticks, the host later simulated the complete
/// versions, and the two worlds disagreed from the join onwards — silently, and for ever. It only
/// showed up with three peers, because with two the only other participant is the host itself,
/// whose actions are always already in the tracker.
///
/// The client is not left short: everything past `current_tick` reaches it through the ordinary
/// [`broadcast_authoritative_actions`] path, which waits for completeness by design.
pub fn broadcast_buffered_authoritative_actions_to_loaded_clients<A: LockstepAction>(
    mut commands: Commands,
    current_tick: Res<CurrentTick>,
    tracker: Res<ActionTracker<A>>,
    mut pending_client_joins: ResMut<PendingClientJoins>,
    lobby_clients: Query<(Entity, &LobbyClientPlayerUuid), With<LobbyClient>>,
    mut messages: MessageReader<ReceivedEnsembleMessage<crate::ClientLoaded>>,
) {
    let end_tick = current_tick.0;
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
            // A tick with no entry is an *empty* tick, not an absent one, and the difference is a
            // hung session: a host's first `host_tick_buffer` ticks have no entries at all,
            // because its own flush schedules that far ahead. Skipping them left a client whose
            // snapshot landed in that window waiting on a tick that would never be sent.
            let players_actions = tracker
                .ticks
                .get(&tick)
                .map(|players_actions| {
                    players_actions
                        .iter()
                        .map(|(k, v)| (*k, v.clone()))
                        .collect()
                })
                .unwrap_or_default();

            let message = AuthoritativeTick {
                tick,
                players_actions,
            };
            // Deliberately the coalescing default, unlike the steady-state broadcast. This loop
            // emits one message per missed tick and the range is however long the join took, so it
            // is a burst of hundreds of small messages in one frame -- the one shape Nagle is
            // actually for. Sending each as its own datagram here invites the loss that a reliable
            // ordered channel answers with head-of-line blocking, at the exact moment this client
            // is trying to catch up.
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
        // An absent entry is an *empty* tick, not an unfinished one. The host simulated this tick,
        // so by definition nothing was outstanding for it — and its own first `host_tick_buffer`
        // ticks have no entries at all, because its flush schedules that far ahead. Breaking here
        // meant those ticks were never broadcast, so a client whose snapshot landed in that window
        // waited on a tick that would never be sent. The per-participant check below still decides
        // whether the tick is genuinely ready to go out.
        let empty = Default::default();
        let players_actions = tracker.ticks.get(&tick).unwrap_or(&empty);

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
        // Not held back to be packed: every client's simulation is stopped until this lands, and
        // the next one is a tick away, so a coalescing send waits for company that never comes and
        // charges every client for it. The catch-up path above is the opposite case and stays on
        // the default -- see `broadcast_buffered_authoritative_actions_to_loaded_clients`.
        commands
            .entity(*host_lobby)
            .trigger(move |entity| LobbyMessage::new_no_delay(entity, message));
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

/// Record a received [`AuthoritativeTick`] in the tracker.
///
/// # Why the tick is registered before the loop
///
/// The client's pause check asks `tracker.ticks.contains_key(&next_tick)` — "have I received
/// tick N" — so what the tracker has to hold is the *arrival* of a tick, not just its contents.
/// Those are different for an empty tick, and inserting only per-player entries would record a
/// tick nobody acted on as though it had never been sent. The client would then wait on it for
/// ever: the host has simulated past it and will not repeat it.
///
/// `broadcast_authoritative_actions` can already produce such a message — it builds one from an
/// absent tracker entry deliberately, since an absent entry is an empty tick rather than an
/// unfinished one. What has kept the wire full so far is only that every participant is
/// required from `joined_at_tick` onwards, so the host's own entry is always in there. That is a
/// liveness guarantee resting on the definition of "required participant", one edit away from
/// spectators or a mid-session leave. Registering the key here does not depend on it.
pub fn apply_authoritative_tick<A: Clone>(
    tracker: &mut ActionTracker<A>,
    authoritative_tick: &AuthoritativeTick<A>,
) {
    tracker.ticks.entry(authoritative_tick.tick).or_default();

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tick_nobody_acted_on_still_counts_as_received() {
        let mut tracker = ActionTracker::<u8>::default();

        apply_authoritative_tick(
            &mut tracker,
            &AuthoritativeTick {
                tick: 7,
                players_actions: Vec::new(),
            },
        );

        assert!(
            tracker.ticks.contains_key(&7),
            "an empty authoritative tick has to be distinguishable from one that never arrived — \
             the client's pause check reads exactly this key, and would otherwise wait for ever \
             on a tick the host has already simulated past"
        );
        assert!(
            tracker.ticks[&7].is_empty(),
            "registering the tick must not invent an actor for it"
        );
    }

    #[test]
    fn a_tick_with_actions_records_them_as_well_as_the_tick() {
        let mut tracker = ActionTracker::<u8>::default();

        apply_authoritative_tick(
            &mut tracker,
            &AuthoritativeTick {
                tick: 3,
                players_actions: vec![(11, vec![1, 2]), (22, Vec::new())],
            },
        );

        assert_eq!(tracker.ticks[&3][&11], vec![1, 2]);
        assert!(
            tracker.ticks[&3].contains_key(&22),
            "a participant who acted on nothing is still present for the tick"
        );
    }
}
