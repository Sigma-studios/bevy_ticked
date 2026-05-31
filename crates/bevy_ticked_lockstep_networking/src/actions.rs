use crate::{
    ActionTracker, ClientScheduledActions, ClientSnapshotState, JoinSnapshot, LocalPendingActions,
    LockstepAction, LockstepConfig,
};
use bevy::prelude::*;
use bevy_ensemble::{Host, Lobby, LobbyMessage, LocalMultiplayerPlayerId, ReceivedEnsembleMessage};
use bevy_ticked::tick::{CurrentTick, TicksPaused};

pub fn insert_actions_into_tracker<A>(
    tracker: &mut ActionTracker<A>,
    tick: u64,
    player_uuid: u128,
    actions: Vec<A>,
) {
    tracker
        .ticks
        .entry(tick)
        .or_default()
        .insert(player_uuid, actions);
}

/// Flush pending local actions into the tracker (host) or send to host (client).
///
/// Runs in `FixedUpdate::TickedSet::PreTick`. Each invocation corresponds to one
/// upcoming tick advancement. Actions are scheduled `buffer` ticks ahead.
pub fn flush_pending_actions<A: LockstepAction, S: JoinSnapshot>(
    ticks_paused: Option<Res<TicksPaused>>,
    pending_actions: Option<ResMut<LocalPendingActions<A>>>,
    snapshot_state: Option<Res<ClientSnapshotState<S>>>,
    config: Res<LockstepConfig>,
    current_tick: Res<CurrentTick>,
    mut tracker: ResMut<ActionTracker<A>>,
    local_player_id: Option<Res<LocalMultiplayerPlayerId>>,
    host_lobbies: Query<(), (With<Lobby>, With<Host>)>,
    client_lobby: Option<Single<Entity, (With<Lobby>, Without<Host>)>>,
    mut commands: Commands,
) {
    if ticks_paused.is_some() {
        return;
    }

    let Some(mut pending_actions) = pending_actions else {
        return;
    };

    let is_host = !host_lobbies.is_empty();
    let local_player_uuid = local_player_id.as_ref().map(|p| p.0).unwrap_or(0);
    let client_snapshot_ready = snapshot_state.as_ref().is_none_or(|state| state.ready);

    if client_lobby.is_some() && !client_snapshot_ready {
        return;
    }

    let actions = std::mem::take(&mut pending_actions.0);
    // The next tick that will run is current_tick.0 + 1
    let next_tick = current_tick.0 + 1;

    if let Some(ref client_lobby) = client_lobby {
        let scheduled_tick = next_tick + config.client_tick_buffer;
        let message = ClientScheduledActions {
            tick: scheduled_tick,
            actions,
        };
        commands
            .entity(**client_lobby)
            .trigger(move |entity| LobbyMessage::new(entity, message));
        return;
    }

    if is_host {
        insert_actions_into_tracker(
            &mut tracker,
            next_tick + config.host_tick_buffer,
            local_player_uuid,
            actions,
        );
        return;
    }

    // Single-player / no lobby: insert directly at the next tick
    if !actions.is_empty() {
        insert_actions_into_tracker(&mut tracker, next_tick, local_player_uuid, actions);
    }
}

pub fn receive_client_actions<A: LockstepAction>(
    mut messages: MessageReader<ReceivedEnsembleMessage<ClientScheduledActions<A>>>,
    mut tracker: ResMut<ActionTracker<A>>,
    host_lobbies: Query<(), (With<Lobby>, With<Host>)>,
) {
    if host_lobbies.is_empty() {
        return;
    }

    for message in messages.read() {
        let Some(sender) = message.sender else {
            continue;
        };
        insert_actions_into_tracker(
            &mut tracker,
            message.message.tick,
            sender,
            message.message.actions.clone(),
        );
    }
}
