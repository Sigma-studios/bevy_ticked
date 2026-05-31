pub mod actions;
pub mod authoritative;
pub mod join;
pub mod messages;
pub mod participants;
pub mod pause;
pub mod plugin;
pub mod prelude;
pub mod resources;

pub use actions::{flush_pending_actions, insert_actions_into_tracker, receive_client_actions};
pub use authoritative::{
    broadcast_authoritative_actions, broadcast_buffered_authoritative_actions_to_loaded_clients,
    cleanup_old_tracker_entries, receive_authoritative_actions,
    replay_stashed_authoritative_actions, tracker_has_actions_for_player,
};
pub use join::{
    flush_provided_join_snapshots, receive_join_snapshot_requests, receive_join_snapshot_responses,
    request_join_snapshot_on_client_join, send_client_loaded_after_snapshot_applied,
};
pub use messages::{
    ApplyJoinSnapshot, AuthoritativeTick, CaptureJoinSnapshot, ClientLoaded,
    ClientScheduledActions, JoinSnapshotApplied, JoinSnapshotRequest, JoinSnapshotResponse,
    ParticipantJoined, ProvideJoinSnapshot,
};
pub use participants::{
    LockstepLobbyParticipant, PendingLockstepParticipantJoins, activate_loaded_client_participants,
    add_host_participant, apply_pending_lockstep_participants, apply_received_participants,
    broadcast_new_participants_to_existing_clients, broadcast_participants_to_loaded_clients,
    participant_is_required_for_tick,
};
pub use pause::sync_lockstep_pause_state;
pub use plugin::{LockstepConfig, LockstepJoinSet, LockstepPlugin};
pub use resources::{
    ActionTracker, ClientSnapshotState, LastBroadcastTick, LocalPendingActions, PendingClientJoins,
    PendingJoinSnapshotFlushes, StashedAuthoritativeTicks,
};

use serde::{Serialize, de::DeserializeOwned};

pub trait LockstepAction: Serialize + DeserializeOwned + Clone + Send + Sync + 'static {}

impl<T> LockstepAction for T where T: Serialize + DeserializeOwned + Clone + Send + Sync + 'static {}

pub trait JoinSnapshot: Serialize + DeserializeOwned + Clone + Send + Sync + 'static {}

impl<T> JoinSnapshot for T where T: Serialize + DeserializeOwned + Clone + Send + Sync + 'static {}
