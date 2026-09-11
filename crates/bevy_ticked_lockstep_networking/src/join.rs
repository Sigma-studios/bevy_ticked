use crate::{
    ApplyJoinSnapshot, CaptureJoinSnapshot, ClientLoaded, ClientSnapshotState,
    JOIN_SNAPSHOT_REQUEST_INTERVAL, JoinSnapshot, JoinSnapshotApplied, JoinSnapshotReceived,
    JoinSnapshotRequest, JoinSnapshotResponse, LastJoinSnapshotRequests, LockstepConfig,
    LockstepLobbyParticipant, PendingClientJoins, PendingJoinSnapshotFlushes, ProvideJoinSnapshot,
};
use bevy::prelude::*;
use bevy_ensemble::{
    Host, Lobby, LobbyClient, LobbyClientMessage, LobbyClientPlayerUuid, LobbyMessage,
    LobbyParticipant, LobbyParticipantOf, ReceivedEnsembleMessage,
};
use bevy_ticked::tick::CurrentTick;
use std::marker::PhantomData;

pub fn request_join_snapshot_on_client_join<S: JoinSnapshot>(
    mut commands: Commands,
    joined_lobbies: Query<Entity, (Added<Lobby>, Without<Host>)>,
    mut snapshot_state: ResMut<ClientSnapshotState<S>>,
) {
    for lobby in joined_lobbies.iter() {
        snapshot_state.ready = false;
        commands
            .entity(lobby)
            .trigger(|entity| LobbyMessage::new(entity, JoinSnapshotRequest));
    }
}

/// Host: capture a snapshot for a client that asked for one — once per client, per second.
///
/// A capture is the most expensive thing a client can make the host do with one message, and
/// the response is the largest one the host sends. Nothing bounded either: a client that sent
/// the request in a loop had the host walking its world every frame and the link full of
/// snapshots. And a client that received two snapshots applied both, the second one clearing
/// the tracker of every authoritative tick that had arrived in between — so a duplicate request
/// from an honest client was a hung join, not merely a wasted capture.
///
/// So: a request from a uuid that is already a participant is ignored (it has loaded; a second
/// snapshot cannot help it), and a request from a uuid whose last capture was less than
/// [`JOIN_SNAPSHOT_REQUEST_INTERVAL`] ago is answered by that capture. A request after the
/// interval is a genuine retry and is answered afresh; its snapshot tick is the newer one, but
/// the tracker floor stays at the older, so a client that applies either is caught up from it.
pub fn receive_join_snapshot_requests<S: JoinSnapshot>(
    mut messages: MessageReader<ReceivedEnsembleMessage<JoinSnapshotRequest>>,
    host_lobby: Option<Single<Entity, (With<Lobby>, With<Host>)>>,
    participants: Query<(&LobbyParticipant, &LobbyParticipantOf), With<LockstepLobbyParticipant>>,
    mut pending_client_joins: ResMut<PendingClientJoins>,
    mut last_requests: ResMut<LastJoinSnapshotRequests>,
    time: Res<Time>,
    current_tick: Res<CurrentTick>,
    mut capture_messages: MessageWriter<CaptureJoinSnapshot<S>>,
) {
    let Some(host_lobby) = host_lobby else {
        return;
    };

    for message in messages.read() {
        let Some(sender) = message.sender else {
            continue;
        };

        if participants
            .iter()
            .any(|(participant, of)| of.0 == *host_lobby && participant.player_uuid == sender)
        {
            warn!("ignoring a join snapshot request from {sender}, which is already a participant");
            continue;
        }

        let now = time.elapsed();
        if let Some(last) = last_requests.0.get(&sender)
            && now.saturating_sub(*last) < JOIN_SNAPSHOT_REQUEST_INTERVAL
        {
            debug!("ignoring a repeated join snapshot request from {sender}");
            continue;
        }
        last_requests.0.insert(sender, now);

        let snapshot_tick = current_tick.0;
        pending_client_joins
            .0
            .entry(sender)
            .or_insert(snapshot_tick);

        capture_messages.write(CaptureJoinSnapshot {
            requester: sender,
            snapshot_tick,
            marker: PhantomData,
        });
    }
}

fn try_send_snapshot_response<S: JoinSnapshot>(
    commands: &mut Commands,
    requester: u128,
    response: &JoinSnapshotResponse<S>,
    lobby_clients: &Query<(Entity, &LobbyClientPlayerUuid), With<LobbyClient>>,
) -> bool {
    let Some((client_entity, _)) = lobby_clients
        .iter()
        .find(|(_, player_uuid)| player_uuid.0 == requester)
    else {
        return false;
    };

    let response = response.clone();
    commands
        .entity(client_entity)
        .trigger(move |entity| LobbyClientMessage::new(entity, response));
    true
}

pub fn flush_provided_join_snapshots<S: JoinSnapshot>(
    mut commands: Commands,
    mut messages: MessageReader<ProvideJoinSnapshot<S>>,
    mut pending_flushes: ResMut<PendingJoinSnapshotFlushes<S>>,
    lobby_clients: Query<(Entity, &LobbyClientPlayerUuid), With<LobbyClient>>,
) {
    // Retry any previously buffered snapshots
    pending_flushes.pending.retain(|(requester, response)| {
        !try_send_snapshot_response(&mut commands, *requester, response, &lobby_clients)
    });

    // Process new snapshots
    for message in messages.read() {
        let response = JoinSnapshotResponse {
            snapshot_tick: message.snapshot_tick,
            snapshot: message.snapshot.clone(),
        };
        if !try_send_snapshot_response(&mut commands, message.requester, &response, &lobby_clients)
        {
            pending_flushes.pending.push((message.requester, response));
        }
    }
}

pub fn receive_join_snapshot_responses<S: JoinSnapshot>(
    mut messages: MessageReader<ReceivedEnsembleMessage<JoinSnapshotResponse<S>>>,
    client_lobbies: Query<(), (With<Lobby>, Without<Host>)>,
    mut snapshot_state: ResMut<ClientSnapshotState<S>>,
    mut last_scheduled: ResMut<crate::LastScheduledTick>,
    mut apply_messages: MessageWriter<ApplyJoinSnapshot<S>>,
    mut received: MessageWriter<JoinSnapshotReceived>,
) {
    if client_lobbies.is_empty() {
        return;
    }

    for message in messages.read() {
        snapshot_state.ready = false;
        // The snapshot moves this peer's clock, so any tick it scheduled before belongs to a
        // different timeline. `send_client_loaded_after_snapshot_applied` restarts the sequence
        // from the snapshot's tick once the game has applied it.
        last_scheduled.0 = None;
        received.write(JoinSnapshotReceived {
            snapshot_tick: message.message.snapshot_tick,
        });
        apply_messages.write(ApplyJoinSnapshot {
            snapshot_tick: message.message.snapshot_tick,
            snapshot: message.message.snapshot.clone(),
        });
    }
}

/// Client: the game has applied the snapshot; tell the host, and start scheduling from its tick.
///
/// # Why the sequence starts at the snapshot's tick and not at the first flush
///
/// `flush_pending_actions` fills every tick between the last scheduled one and the next, so the
/// host never waits on a hole. With nothing scheduled yet it fills nothing, and the joiner's
/// first batch landed at `snapshot_tick + 1 + client_tick_buffer` with nothing before it. The
/// host, meanwhile, started requiring the joiner's actions from `joined_at_tick +
/// host_tick_buffer + 1`. When the joiner's buffer was the larger of the two — a client whose
/// buffer the adaptive tuner had grown on the previous session's link, joining a LAN host — the
/// first tick the host required was before the first tick the joiner ever scheduled, and the
/// host waited for it for ever.
///
/// Starting the sequence at the snapshot's tick makes the first flush fill from there. The
/// batches for ticks the host has already simulated are dropped on arrival (they are late by
/// construction); the ones for ticks it has not are exactly what it needs.
pub fn send_client_loaded_after_snapshot_applied<S: JoinSnapshot>(
    mut commands: Commands,
    mut messages: MessageReader<JoinSnapshotApplied<S>>,
    client_lobby: Option<Single<Entity, (With<Lobby>, Without<Host>)>>,
    config: Res<LockstepConfig>,
    mut snapshot_state: ResMut<ClientSnapshotState<S>>,
    mut last_scheduled: ResMut<crate::LastScheduledTick>,
) {
    let Some(client_lobby) = client_lobby else {
        return;
    };

    for message in messages.read() {
        last_scheduled.0 = Some(message.snapshot_tick);
        let loaded = ClientLoaded {
            buffer: config.client_tick_buffer,
        };
        commands
            .entity(*client_lobby)
            .trigger(move |entity| LobbyMessage::new(entity, loaded));
        snapshot_state.ready = true;
    }
}

/// Host: forget everything held on behalf of a client whose connection is gone.
///
/// Read off the `LobbyClient` on `Remove` because that is the one place every way a client
/// can go — kicked, timed out, left — meets, and its uuid is still on the entity there. A
/// pending join that outlived its client used to pin the tracker's floor for the rest of the
/// session.
pub fn forget_departed_client_joins<S: JoinSnapshot>(
    removed: On<Remove, LobbyClient>,
    uuids: Query<&LobbyClientPlayerUuid>,
    mut pending_client_joins: ResMut<PendingClientJoins>,
    mut last_requests: ResMut<LastJoinSnapshotRequests>,
    mut pending_flushes: ResMut<PendingJoinSnapshotFlushes<S>>,
) {
    let Ok(uuid) = uuids.get(removed.entity) else {
        return;
    };
    pending_client_joins.0.remove(&uuid.0);
    last_requests.0.remove(&uuid.0);
    pending_flushes
        .pending
        .retain(|(requester, _)| *requester != uuid.0);
}
