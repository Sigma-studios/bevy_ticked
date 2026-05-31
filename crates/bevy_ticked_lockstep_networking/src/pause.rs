use crate::{
    ActionTracker, ClientSnapshotState, JoinSnapshot, LockstepAction, LockstepLobbyParticipant,
    participant_is_required_for_tick, tracker_has_actions_for_player,
};
use bevy::prelude::*;
use bevy_ensemble::{Host, Lobby, LobbyParticipant, LobbyParticipantOf};
use bevy_ticked::tick::{CurrentTick, TicksPaused};

/// Exclusive system that runs in `FixedUpdate::PreTick` every iteration.
///
/// Because it directly inserts/removes `TicksPaused` on the world (not via
/// deferred commands), `advance_tick_system` in the subsequent `Tick` phase
/// sees the change immediately. This prevents the client from overshooting
/// past available authoritative ticks during catch-up bursts.
pub fn sync_lockstep_pause_state<A: LockstepAction, S: JoinSnapshot>(world: &mut World) {
    let mut lobby_query = world.query_filtered::<(Entity, Option<&Host>), With<Lobby>>();
    let mut host_lobby = None;
    let mut client_lobby = None;
    for (entity, host) in lobby_query.iter(world) {
        if host.is_some() {
            host_lobby = Some(entity);
        } else {
            client_lobby = Some(entity);
        }
    }

    let scoped_lobby = host_lobby.or(client_lobby);
    let Some(scoped_lobby) = scoped_lobby else {
        return;
    };

    // Client must wait for snapshot before ticking
    if client_lobby.is_some() {
        let snapshot_ready = world.resource::<ClientSnapshotState<S>>().ready;
        if !snapshot_ready {
            world.insert_resource(TicksPaused);
            return;
        }
    }

    let current_tick = world.resource::<CurrentTick>().0;
    let next_tick = current_tick + 1;

    let should_pause = if host_lobby.is_some() {
        // Host: wait for every required participant that has previously submitted
        // actions. Participants who have never appeared in the tracker are still
        // joining — their actions are implicitly empty and should not block.
        let required_participant_ids: Vec<u128> = world
            .query::<(&LobbyParticipant, &LockstepLobbyParticipant, &LobbyParticipantOf)>()
            .iter(world)
            .filter(|(_, lockstep, pof)| {
                pof.0 == scoped_lobby && participant_is_required_for_tick(lockstep, next_tick)
            })
            .map(|(p, _, _)| p.player_uuid)
            .collect();

        if required_participant_ids.is_empty() {
            true
        } else {
            let tracker = world.resource::<ActionTracker<A>>();
            required_participant_ids.iter().any(|uuid| {
                let ever_tracked = tracker
                    .ticks
                    .values()
                    .any(|actions| actions.contains_key(uuid));
                // Only block on participants whose actions we have seen before.
                ever_tracked && !tracker_has_actions_for_player(tracker, next_tick, *uuid)
            })
        }
    } else {
        // Client: authoritative ticks from the host are already complete — if the
        // tracker has any data for `next_tick`, all required players are covered.
        // Checking per-player would deadlock when `ParticipantJoined` arrives
        // before the authoritative ticks that include the new participant.
        let has_any_participant = world
            .query::<(&LockstepLobbyParticipant, &LobbyParticipantOf)>()
            .iter(world)
            .any(|(_, pof)| pof.0 == scoped_lobby);

        let tracker = world.resource::<ActionTracker<A>>();
        !has_any_participant || !tracker.ticks.contains_key(&next_tick)
    };

    if should_pause {
        world.insert_resource(TicksPaused);
    } else {
        world.remove_resource::<TicksPaused>();
    }
}
