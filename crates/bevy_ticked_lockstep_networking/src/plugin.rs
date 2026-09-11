use crate::{
    ActionTracker, AdaptiveBufferState, ClientSnapshotState, InitialLockstepConfig, JoinSnapshot,
    LastBroadcastTick, LastJoinSnapshotRequests, LastScheduledTick, LocalPendingActions,
    LockstepAction, PendingClientJoins, PendingJoinSnapshotFlushes,
    PendingLockstepParticipantJoins, StashedAuthoritativeTicks,
    activate_loaded_client_participants, add_host_participant, apply_pending_lockstep_participants,
    apply_received_participants, broadcast_authoritative_actions,
    broadcast_buffered_authoritative_actions_to_loaded_clients,
    broadcast_new_participants_to_existing_clients, broadcast_participants_to_loaded_clients,
    cleanup_old_tracker_entries, flush_pending_actions, flush_provided_join_snapshots,
    forget_departed_client_joins, receive_authoritative_actions, receive_client_actions,
    receive_join_snapshot_requests, receive_join_snapshot_responses,
    replay_stashed_authoritative_actions, request_join_snapshot_on_client_join,
    send_client_loaded_after_snapshot_applied, sync_lockstep_pause_state,
};
use bevy::prelude::*;
use bevy_ensemble::{EnsembleAppExt, EnsembleSet, Lobby, MessageAuthority};
use bevy_ticked::{TickedLoop, TickedSystems};
use std::marker::PhantomData;

#[derive(Resource, Clone, Copy, Debug)]
pub struct LockstepConfig {
    /// The grace window the host gives a client's actions, in ticks: from a client's
    /// `joined_at_tick`, this many ticks of missing actions count as empty before the host waits
    /// on them. Also the window a joiner's `joined_at_tick` is placed after, when the joiner's
    /// own buffer is not larger. Never below one; a zero is clamped at build.
    ///
    /// Not the host's own input lag: the host's actions go into the tick about to run.
    pub host_tick_buffer: u64,
    /// How many ticks ahead of its own clock a client schedules its actions, so that they have
    /// crossed the link and been ruled on before the tick they name is simulated.
    pub client_tick_buffer: u64,
    /// How far past the host's current tick a client may schedule, in ticks.
    ///
    /// Every tick a client names is a tracker entry the host keeps until it simulates that
    /// tick. Without a bound one message could reserve a tick at `u64::MAX`, kept for ever, or
    /// a million of them. 128 is two seconds at 64 Hz — more than the largest buffer the
    /// adaptive tuner will size, with room for a client whose clock has run ahead.
    pub action_horizon: u64,
}

impl Default for LockstepConfig {
    fn default() -> Self {
        Self {
            // One: the smallest grace window. The buffer that matters for the session's
            // rate is the client's, and the host's used to be its own input lag, which it no
            // longer pays.
            host_tick_buffer: 1,
            client_tick_buffer: 6,
            action_horizon: 128,
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

/// Put every piece of session state back to what it was before the lobby existed.
///
/// Including the buffer. The adaptive tuner writes `LockstepConfig`, and what it wrote is a fact
/// about a link that is gone: a client whose buffer had grown to forty on a bad link carried it
/// into the next session, on whatever link that was, and paid the input latency until the tuner
/// had shrunk it back a tick every two seconds. The tuner's smoothed estimate goes with it, or
/// the next session's first samples are averaged into a round trip nobody measured on it.
fn reset_lockstep_state_on_lobby_removed<A: LockstepAction, S: JoinSnapshot>(
    mut removed_lobbies: RemovedComponents<Lobby>,
    mut tracker: ResMut<ActionTracker<A>>,
    pending_actions: Option<ResMut<LocalPendingActions<A>>>,
    mut pending_client_joins: ResMut<PendingClientJoins>,
    mut last_requests: ResMut<LastJoinSnapshotRequests>,
    mut pending_participant_joins: ResMut<PendingLockstepParticipantJoins>,
    mut stashed_ticks: ResMut<StashedAuthoritativeTicks<A>>,
    mut last_broadcast_tick: ResMut<LastBroadcastTick>,
    mut last_scheduled_tick: ResMut<LastScheduledTick>,
    mut snapshot_state: ResMut<ClientSnapshotState<S>>,
    mut pending_snapshot_flushes: ResMut<PendingJoinSnapshotFlushes<S>>,
    initial_config: Res<InitialLockstepConfig>,
    mut config: ResMut<LockstepConfig>,
    adaptive_state: Option<ResMut<AdaptiveBufferState>>,
) {
    if removed_lobbies.read().next().is_none() {
        return;
    }
    tracker.ticks.clear();
    tracker.system.clear();
    if let Some(mut pending_actions) = pending_actions {
        pending_actions.0.clear();
    }
    pending_client_joins.0.clear();
    last_requests.0.clear();
    pending_participant_joins.0.clear();
    stashed_ticks.0.clear();
    last_broadcast_tick.0 = 0;
    // There is no sequence left to stay contiguous with.
    last_scheduled_tick.0 = None;
    snapshot_state.ready = true;
    pending_snapshot_flushes.pending.clear();
    *config = initial_config.0;
    if let Some(mut adaptive_state) = adaptive_state {
        *adaptive_state = AdaptiveBufferState::default();
    }
}

impl<A, S> Plugin for LockstepPlugin<A, S>
where
    A: LockstepAction,
    S: JoinSnapshot,
{
    fn build(&self, app: &mut App) {
        let mut config = self.config;
        if config.host_tick_buffer == 0 {
            // A zero was a hang: the host's pause check required its own entry for the tick its
            // flush had not yet inserted, held, and the flush does not run while held. The
            // host no longer waits on itself, but a client's grace window of zero ticks still
            // asks its first batch to arrive before it can have been sent.
            warn!("LockstepConfig::host_tick_buffer of 0 is clamped to 1");
            config.host_tick_buffer = 1;
        }
        crate::session::install::<A>(app);
        app.insert_resource(crate::session::StageSystemActions(
            crate::session::stage_into_tracker::<A>,
        ));
        app.insert_resource(config)
            .insert_resource(InitialLockstepConfig(config))
            .init_resource::<ActionTracker<A>>()
            .init_resource::<LocalPendingActions<A>>()
            .init_resource::<PendingClientJoins>()
            .init_resource::<LastJoinSnapshotRequests>()
            .init_resource::<PendingLockstepParticipantJoins>()
            .init_resource::<StashedAuthoritativeTicks<A>>()
            .init_resource::<LastBroadcastTick>()
            .init_resource::<LastScheduledTick>()
            .insert_resource(ClientSnapshotState::<S>::default())
            .init_resource::<PendingJoinSnapshotFlushes<S>>()
            .add_message::<crate::CaptureJoinSnapshot<S>>()
            .add_message::<crate::ApplyJoinSnapshot<S>>()
            .add_message::<crate::JoinSnapshotApplied<S>>()
            .add_message::<crate::ProvideJoinSnapshot<S>>()
            .add_message::<crate::JoinSnapshotReceived>()
            .add_message::<crate::ClientAccepted>()
            .add_observer(forget_departed_client_joins::<S>)
            .configure_sets(
                Update,
                (
                    LockstepJoinSet::CaptureJoinSnapshot,
                    LockstepJoinSet::ApplyJoinSnapshot,
                    LockstepJoinSet::FinalizeJoinSnapshot,
                )
                    .chain(),
            )
            // Protocol-level types: never relayed. Whatever the host says about the session —
            // a join snapshot, an authoritative tick, the roster — a client takes from its host
            // and nobody else.
            .register_control_message_type::<crate::JoinSnapshotRequest>(
                "bevy_ticked_lockstep/JoinSnapshotRequest",
                MessageAuthority::Any,
            )
            .register_control_message_type::<crate::JoinSnapshotResponse<S>>(
                "bevy_ticked_lockstep/JoinSnapshotResponse",
                MessageAuthority::HostOnly,
            )
            .register_control_message_type::<crate::ClientLoaded>(
                "bevy_ticked_lockstep/ClientLoaded",
                MessageAuthority::Any,
            )
            .register_control_message_type::<crate::ClientScheduledActions<A>>(
                "bevy_ticked_lockstep/ClientScheduledActions",
                MessageAuthority::Any,
            )
            .register_control_message_type::<crate::AuthoritativeTick<A>>(
                "bevy_ticked_lockstep/AuthoritativeTick",
                MessageAuthority::HostOnly,
            )
            .register_control_message_type::<crate::ParticipantJoined>(
                "bevy_ticked_lockstep/ParticipantJoined",
                MessageAuthority::HostOnly,
            )
            .add_systems(
                TickedLoop,
                (
                    sync_lockstep_pause_state::<A, S>
                        .in_set(TickedSystems::PreTick)
                        .before(flush_pending_actions::<A, S>),
                    flush_pending_actions::<A, S>.in_set(TickedSystems::PreTick),
                    broadcast_authoritative_actions::<A>.in_set(TickedSystems::PostTick),
                    cleanup_old_tracker_entries::<A>
                        .in_set(TickedSystems::PostTick)
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
                    receive_join_snapshot_responses::<S>.before(LockstepJoinSet::ApplyJoinSnapshot),
                    send_client_loaded_after_snapshot_applied::<S>
                        .in_set(LockstepJoinSet::FinalizeJoinSnapshot),
                    activate_loaded_client_participants
                        .before(broadcast_participants_to_loaded_clients)
                        .before(broadcast_new_participants_to_existing_clients)
                        .before(broadcast_buffered_authoritative_actions_to_loaded_clients::<A>),
                    broadcast_participants_to_loaded_clients
                        .before(broadcast_buffered_authoritative_actions_to_loaded_clients::<A>),
                    broadcast_new_participants_to_existing_clients,
                    broadcast_buffered_authoritative_actions_to_loaded_clients::<A>,
                    reset_lockstep_state_on_lobby_removed::<A, S>,
                ),
            )
            // In `PreUpdate`, and this is the whole point of them being here rather than in
            // `Update` with everything else.
            //
            // Bevy's frame runs `First`, `PreUpdate`, `RunFixedMainLoop`, `Update`. The backend
            // drains the socket in `PreUpdate` (`EnsembleSet::ReceivePackets`), and the tick loop
            // is inside `RunFixedMainLoop` — so a reader in `Update` sees a packet only *after*
            // every fixed step of the frame it arrived in has already decided whether to pause.
            //
            // That is a whole frame of latency added to each direction, on the two messages the
            // pause check blocks on. The host had a client's actions sitting in the message queue
            // and stalled anyway; the client had the authoritative tick and waited. At 60fps it is
            // ~16.7ms each way, roughly 33ms on the round trip — against the ~12ms of slack a
            // 50ms link leaves at `host_tick_buffer` 6. It is also invisible to the buffer
            // controller, because `PeerRtt` measures the socket seam and this happens above it.
            //
            // The roster is here for a different reason: a participant has to be on it before
            // the first tick of the frame its `ParticipantJoined` arrived in, or a game that
            // spawns the player inside the tick at `joined_at_tick` spawns it a tick late on
            // this peer and on time on the others. The rest of the join handshake is not on the
            // per-tick critical path and stays in `Update`.
            .add_systems(
                PreUpdate,
                (
                    receive_client_actions::<A>,
                    // Still before `receive_authoritative_actions`, so a tick that arrives in the
                    // same frame the stash drains lands after the stashed ones rather than in the
                    // middle of them.
                    replay_stashed_authoritative_actions::<A, S>
                        .before(receive_authoritative_actions::<A, S>),
                    receive_authoritative_actions::<A, S>,
                    apply_received_participants,
                    apply_pending_lockstep_participants.after(apply_received_participants),
                )
                    .after(EnsembleSet::ReceivePackets),
            );
    }

    fn finish(&self, app: &mut App) {
        // A lockstep client has no prediction lead to steer, but it does dilate its rate to
        // catch up after a join (part 2 of the lockstep phase), and the same clock rule keeps
        // one story for every networked role.
        bevy_ticked::require_steerable_tick_source(app, "LockstepPlugin");
    }
}
