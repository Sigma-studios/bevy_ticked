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

    // Collect required participant UUIDs for the next tick
    let required_participant_ids: Vec<u128> = world
        .query::<(&LobbyParticipant, &LockstepLobbyParticipant, &LobbyParticipantOf)>()
        .iter(world)
        .filter(|(_, lockstep, pof)| {
            pof.0 == scoped_lobby && participant_is_required_for_tick(lockstep, next_tick)
        })
        .map(|(p, _, _)| p.player_uuid)
        .collect();

    let tracker = world.resource::<ActionTracker<A>>();
    let has_missing = required_participant_ids
        .iter()
        .any(|uuid| !tracker_has_actions_for_player(tracker, next_tick, *uuid));

    if required_participant_ids.is_empty() || has_missing {
        world.insert_resource(TicksPaused);
    } else {
        world.remove_resource::<TicksPaused>();
    }
}
