use crate::{
    ActionTracker, ClientSnapshotState, JoinSnapshot, LastBroadcastTick, LocalPendingActions,
    LockstepAction, PendingClientJoins, PendingJoinSnapshotFlushes,
    PendingLockstepParticipantJoins, StashedAuthoritativeTicks,
    activate_loaded_client_participants, add_host_participant,
    apply_pending_lockstep_participants, apply_received_participants,
    broadcast_authoritative_actions,
    broadcast_buffered_authoritative_actions_to_loaded_clients,
    broadcast_new_participants_to_existing_clients, broadcast_participants_to_loaded_clients,
    cleanup_old_tracker_entries, flush_pending_actions, flush_provided_join_snapshots,
    prefill_actions_for_new_participants, receive_authoritative_actions, receive_client_actions,
    receive_join_snapshot_requests, receive_join_snapshot_responses,
    replay_stashed_authoritative_actions, request_join_snapshot_on_client_join,
    send_client_loaded_after_snapshot_applied, sync_lockstep_pause_state,
};
use bevy::prelude::*;
use bevy_ensemble::{EnsembleAppExt, Lobby};
use bevy_ticked::TickedSet;
use std::marker::PhantomData;

#[derive(Resource, Clone, Copy, Debug)]
pub struct LockstepConfig {
    pub host_tick_buffer: u64,
    pub client_tick_buffer: u64,
}

impl Default for LockstepConfig {
    fn default() -> Self {
        Self {
            host_tick_buffer: 6,
            client_tick_buffer: 6,
        }
    }
}

#[derive(SystemSet, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LockstepJoinSet {
    CaptureJoinSnapshot,
    ApplyJoinSnapshot,
    FinalizeJoinSnapshot,
}

pub struct LockstepPlugin<A, S> {
    pub config: LockstepConfig,
    pub marker: PhantomData<fn() -> (A, S)>,
}

impl<A, S> Default for LockstepPlugin<A, S> {
    fn default() -> Self {
        Self {
            config: LockstepConfig::default(),
            marker: PhantomData,
        }
    }
}

fn reset_lockstep_state_on_lobby_removed<A: LockstepAction, S: JoinSnapshot>(
    mut removed_lobbies: RemovedComponents<Lobby>,
    mut tracker: ResMut<ActionTracker<A>>,
    mut pending_actions: ResMut<LocalPendingActions<A>>,
    mut pending_client_joins: ResMut<PendingClientJoins>,
    mut pending_participant_joins: ResMut<PendingLockstepParticipantJoins>,
    mut stashed_ticks: ResMut<StashedAuthoritativeTicks<A>>,
    mut last_broadcast_tick: ResMut<LastBroadcastTick>,
    mut snapshot_state: ResMut<ClientSnapshotState<S>>,
    mut pending_snapshot_flushes: ResMut<PendingJoinSnapshotFlushes<S>>,
) {
    if removed_lobbies.read().next().is_none() {
        return;
    }
    tracker.ticks.clear();
    pending_actions.0.clear();
    pending_client_joins.0.clear();
    pending_participant_joins.0.clear();
    stashed_ticks.0.clear();
    last_broadcast_tick.0 = 0;
    snapshot_state.ready = true;
    pending_snapshot_flushes.pending.clear();
}

impl<A, S> Plugin for LockstepPlugin<A, S>
where
    A: LockstepAction,
    S: JoinSnapshot,
{
    fn build(&self, app: &mut App) {
        app.insert_resource(self.config)
            .init_resource::<ActionTracker<A>>()
            .init_resource::<LocalPendingActions<A>>()
            .init_resource::<PendingClientJoins>()
            .init_resource::<PendingLockstepParticipantJoins>()
            .init_resource::<StashedAuthoritativeTicks<A>>()
            .init_resource::<LastBroadcastTick>()
            .insert_resource(ClientSnapshotState::<S>::default())
            .init_resource::<PendingJoinSnapshotFlushes<S>>()
            .add_message::<crate::CaptureJoinSnapshot<S>>()
            .add_message::<crate::ApplyJoinSnapshot<S>>()
            .add_message::<crate::JoinSnapshotApplied<S>>()
            .add_message::<crate::ProvideJoinSnapshot<S>>()
            .configure_sets(
                Update,
                (
                    LockstepJoinSet::CaptureJoinSnapshot,
                    LockstepJoinSet::ApplyJoinSnapshot,
                    LockstepJoinSet::FinalizeJoinSnapshot,
                )
                    .chain(),
            )
            .register_ensemble_message_type::<crate::JoinSnapshotRequest>()
            .register_ensemble_message_type::<crate::JoinSnapshotResponse<S>>()
            .register_ensemble_message_type::<crate::ClientLoaded>()
            .register_ensemble_message_type::<crate::ClientScheduledActions<A>>()
            .register_ensemble_message_type::<crate::AuthoritativeTick<A>>()
            .register_ensemble_message_type::<crate::ParticipantJoined>()
            .add_systems(
                FixedUpdate,
                (
                    sync_lockstep_pause_state::<A, S>
                        .in_set(TickedSet::PreTick)
                        .before(flush_pending_actions::<A, S>),
                    flush_pending_actions::<A, S>.in_set(TickedSet::PreTick),
                    broadcast_authoritative_actions::<A>.in_set(TickedSet::PostTick),
                    cleanup_old_tracker_entries::<A>
                        .in_set(TickedSet::PostTick)
                        .after(broadcast_authoritative_actions::<A>),
                ),
            )
            .add_systems(
                Update,
                (
                    add_host_participant,
                    request_join_snapshot_on_client_join::<S>,
                    receive_join_snapshot_requests::<S>
                        .before(LockstepJoinSet::CaptureJoinSnapshot),
                    flush_provided_join_snapshots::<S>
                        .after(LockstepJoinSet::CaptureJoinSnapshot)
                        .before(LockstepJoinSet::ApplyJoinSnapshot),
                    receive_join_snapshot_responses::<S>
                        .before(LockstepJoinSet::ApplyJoinSnapshot),
                    send_client_loaded_after_snapshot_applied::<S>
                        .in_set(LockstepJoinSet::FinalizeJoinSnapshot),
                    activate_loaded_client_participants
                        .before(broadcast_participants_to_loaded_clients)
                        .before(broadcast_new_participants_to_existing_clients)
                        .before(
                            broadcast_buffered_authoritative_actions_to_loaded_clients::<A>,
                        ),
                    broadcast_participants_to_loaded_clients
                        .before(
                            broadcast_buffered_authoritative_actions_to_loaded_clients::<A>,
                        ),
                    broadcast_new_participants_to_existing_clients,
                    apply_received_participants,
                    apply_pending_lockstep_participants.after(apply_received_participants),
                    prefill_actions_for_new_participants::<A>,
                    broadcast_buffered_authoritative_actions_to_loaded_clients::<A>,
                    receive_client_actions::<A>,
                    replay_stashed_authoritative_actions::<A, S>
                        .before(receive_authoritative_actions::<A, S>),
                    receive_authoritative_actions::<A, S>,
                    reset_lockstep_state_on_lobby_removed::<A, S>,
                ),
            );
    }
}
