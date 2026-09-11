use crate::{
    AuthoritativeTick, JoinSnapshot, JoinSnapshotResponse, LockstepConfig, LockstepPauseReason,
    SystemAction,
};
use bevy::prelude::*;
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    marker::PhantomData,
    time::Duration,
};

#[derive(Resource)]
pub struct LocalPendingActions<A>(pub Vec<A>);

impl<A> Default for LocalPendingActions<A> {
    fn default() -> Self {
        Self(Vec::new())
    }
}

#[derive(Resource)]
pub struct ActionTracker<A> {
    pub ticks: HashMap<u64, BTreeMap<u128, Vec<A>>>,
    /// The session's own actions per tick, ruled on by the host with the players'.
    pub system: HashMap<u64, Vec<SystemAction>>,
}

impl<A> ActionTracker<A> {
    pub fn actions_for_tick(&self, tick: u64) -> Option<&BTreeMap<u128, Vec<A>>> {
        self.ticks.get(&tick)
    }

    /// The session's actions on `tick`, empty if none.
    pub fn system_actions_for_tick(&self, tick: u64) -> &[SystemAction] {
        self.system.get(&tick).map(Vec::as_slice).unwrap_or(&[])
    }

    /// The newest tick the tracker holds anything for.
    pub fn newest_tick(&self) -> Option<u64> {
        self.ticks
            .keys()
            .chain(self.system.keys())
            .copied()
            .max()
    }
}

impl<A> Default for ActionTracker<A> {
    fn default() -> Self {
        Self {
            ticks: HashMap::new(),
            system: HashMap::new(),
        }
    }
}

/// Host side: session actions waiting to be ruled into the next tick.
#[derive(Resource, Default, Debug, Clone)]
pub struct PendingSystemActions(pub Vec<SystemAction>);

/// Who is in the simulation, as of the ticks this peer has run. Changes only inside a tick,
/// from the tick's [`SystemAction`]s, so every peer changes it on the same tick.
#[derive(Resource, Default, Debug, Clone, PartialEq, Eq)]
pub struct LockstepRoster(pub BTreeSet<u128>);

/// Whether the session is paused, as of the ticks this peer has run.
#[derive(Resource, Default, Debug, Clone, Copy, PartialEq, Eq)]
pub struct LockstepPaused(pub Option<LockstepPauseReason>);

/// What the host does about a participant that stops answering.
#[derive(Resource, Clone, Copy, Debug, PartialEq, Eq)]
pub struct StallPolicy {
    /// After this long waiting on a participant, [`LockstepStall::paused`] is set for the
    /// UI: the session is, in effect, paused for them.
    pub pause_after: Duration,
    /// After this long, the host kicks the participant: everyone sees them leave on the same
    /// tick and the session runs again. `None` waits for ever.
    pub kick_after: Option<Duration>,
}

impl Default for StallPolicy {
    fn default() -> Self {
        Self {
            pause_after: Duration::from_secs(1),
            kick_after: Some(Duration::from_secs(10)),
        }
    }
}

/// Who this peer is waiting on, and for how long. For an overlay: "waiting for Alice (3 s)".
#[derive(Resource, Default, Debug, Clone, PartialEq, Eq)]
pub struct LockstepStall {
    /// The participants whose actions the next tick needs and does not have (host), or the
    /// host, when the next authoritative tick has not come (client).
    pub waiting_on: Vec<u128>,
    /// How long this has been going on, on the frame clock.
    pub since: Duration,
    /// Past [`StallPolicy::pause_after`].
    pub paused: bool,
}

/// Client side: this peer's own input-arrival margin, as the host last reported it.
#[derive(Resource, Default, Debug, Clone, Copy, PartialEq, Eq)]
pub struct OwnInputMargin(pub Option<i16>);

/// Host side: the newest arrival margin per client.
#[derive(Resource, Default, Debug, Clone, PartialEq, Eq)]
pub struct ArrivalMargins(pub BTreeMap<u128, i16>);

/// Host side: clients that have been sent a join snapshot and have not yet said `ClientLoaded`,
/// by the tick their snapshot described.
///
/// While a uuid is in here the tracker keeps every tick after its snapshot, so the catch-up can
/// be sent once the client loads. An entry leaves when the client loads, when its `LobbyClient`
/// goes, or when it has been pending longer than
/// [`PENDING_JOIN_WINDOW_TICKS`](crate::PENDING_JOIN_WINDOW_TICKS) — the last because a client
/// that requested a snapshot and then went quiet used to pin the tracker's floor for the rest of
/// the session, and the tracker grew by one tick's worth of everybody's actions per tick until
/// the host ran out of memory.
#[derive(Resource, Default)]
pub struct PendingClientJoins(pub HashMap<u128, u64>);

/// Host side: when each client last had a join snapshot captured for it, on the frame clock.
///
/// A capture walks the world and the response is the largest message this crate sends, so a
/// client that asks again and again is a client that can make the host do that work again and
/// again. Repeats inside [`JOIN_SNAPSHOT_REQUEST_INTERVAL`] are answered by the capture already
/// under way.
#[derive(Resource, Default)]
pub struct LastJoinSnapshotRequests(pub HashMap<u128, Duration>);

/// How long a second `JoinSnapshotRequest` from the same client is treated as a repeat of the
/// first rather than a new request.
pub const JOIN_SNAPSHOT_REQUEST_INTERVAL: Duration = Duration::from_secs(1);

/// The `LockstepConfig` the plugin was built with, so a session that ends can put it back.
///
/// The adaptive buffer writes the live config, and what it wrote is a fact about a link that no
/// longer exists once the lobby goes. A client that had grown to forty ticks of buffer on a bad
/// link carried those forty into the next lobby it joined, on whatever link that was, and paid
/// the input latency until the tuner had shrunk them back a tick every two seconds.
#[derive(Resource, Clone, Copy, Debug)]
pub struct InitialLockstepConfig(pub LockstepConfig);

#[derive(Resource)]
pub struct StashedAuthoritativeTicks<A>(pub Vec<AuthoritativeTick<A>>);

impl<A> Default for StashedAuthoritativeTicks<A> {
    fn default() -> Self {
        Self(Vec::new())
    }
}

#[derive(Resource, Default)]
pub struct LastBroadcastTick(pub u64);

#[derive(Resource)]
pub struct ClientSnapshotState<S: JoinSnapshot> {
    pub ready: bool,
    pub marker: PhantomData<fn() -> S>,
}

impl<S: JoinSnapshot> Default for ClientSnapshotState<S> {
    fn default() -> Self {
        Self {
            ready: true,
            marker: PhantomData,
        }
    }
}

/// Buffers snapshot responses that couldn't be sent because the
/// `LobbyClient` entity wasn't found yet (ensemble scheduling race).
#[derive(Resource)]
pub struct PendingJoinSnapshotFlushes<S: JoinSnapshot> {
    pub pending: Vec<(u128, JoinSnapshotResponse<S>)>,
}

impl<S: JoinSnapshot> Default for PendingJoinSnapshotFlushes<S> {
    fn default() -> Self {
        Self {
            pending: Vec::new(),
        }
    }
}
