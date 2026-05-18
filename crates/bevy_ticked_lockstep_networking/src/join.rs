use crate::{
    ApplyJoinSnapshot, CaptureJoinSnapshot, ClientLoaded, ClientSnapshotState, JoinSnapshot,
    JoinSnapshotApplied, JoinSnapshotRequest, JoinSnapshotResponse,
    PendingClientJoins, PendingJoinSnapshotFlushes, ProvideJoinSnapshot,
};
use bevy::prelude::*;
use bevy_ensemble::{
    Host, Lobby, LobbyClient, LobbyClientMessage, LobbyClientPlayerUuid, LobbyMessage,
    ReceivedEnsembleMessage,
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

pub fn receive_join_snapshot_requests<S: JoinSnapshot>(
    mut messages: MessageReader<ReceivedEnsembleMessage<JoinSnapshotRequest>>,
    host_lobby: Option<Single<Entity, (With<Lobby>, With<Host>)>>,
    mut pending_client_joins: ResMut<PendingClientJoins>,
    current_tick: Res<CurrentTick>,
    mut capture_messages: MessageWriter<CaptureJoinSnapshot<S>>,
) {
    let Some(_host_lobby) = host_lobby else {
        return;
    };

    for message in messages.read() {
        let Some(sender) = message.sender else {
            continue;
        };

        let snapshot_tick = current_tick.0;
        pending_client_joins.0.entry(sender).or_insert(snapshot_tick);

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
    mut apply_messages: MessageWriter<ApplyJoinSnapshot<S>>,
) {
    if client_lobbies.is_empty() {
        return;
    }

    for message in messages.read() {
        snapshot_state.ready = false;
        apply_messages.write(ApplyJoinSnapshot {
            snapshot_tick: message.message.snapshot_tick,
            snapshot: message.message.snapshot.clone(),
        });
    }
}

pub fn send_client_loaded_after_snapshot_applied<S: JoinSnapshot>(
    mut commands: Commands,
    mut messages: MessageReader<JoinSnapshotApplied<S>>,
    client_lobby: Option<Single<Entity, (With<Lobby>, Without<Host>)>>,
    mut snapshot_state: ResMut<ClientSnapshotState<S>>,
) {
    let Some(client_lobby) = client_lobby else {
        return;
    };

    for _message in messages.read() {
        commands
            .entity(*client_lobby)
            .trigger(|entity| LobbyMessage::new(entity, ClientLoaded));
        snapshot_state.ready = true;
    }
}
