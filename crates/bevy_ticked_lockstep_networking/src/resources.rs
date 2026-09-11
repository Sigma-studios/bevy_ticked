use crate::{AuthoritativeTick, JoinSnapshot, JoinSnapshotResponse, LockstepConfig};
use bevy::prelude::*;
use std::{
    collections::{BTreeMap, HashMap},
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
}

impl<A> ActionTracker<A> {
    pub fn actions_for_tick(&self, tick: u64) -> Option<&BTreeMap<u128, Vec<A>>> {
        self.ticks.get(&tick)
    }
}

impl<A> Default for ActionTracker<A> {
    fn default() -> Self {
        Self {
            ticks: HashMap::new(),
        }
    }
}

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
