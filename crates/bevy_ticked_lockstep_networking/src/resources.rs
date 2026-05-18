use crate::{AuthoritativeTick, JoinSnapshot, JoinSnapshotResponse};
use bevy::prelude::*;
use std::{collections::HashMap, marker::PhantomData};

#[derive(Resource)]
pub struct LocalPendingActions<A>(pub Vec<A>);

impl<A> Default for LocalPendingActions<A> {
    fn default() -> Self {
        Self(Vec::new())
    }
}

#[derive(Resource)]
pub struct ActionTracker<A> {
    pub ticks: HashMap<u64, Vec<(u128, Vec<A>)>>,
}

impl<A> ActionTracker<A> {
    pub fn actions_for_tick(&self, tick: u64) -> Option<&[(u128, Vec<A>)]> {
        self.ticks.get(&tick).map(Vec::as_slice)
    }
}

impl<A> Default for ActionTracker<A> {
    fn default() -> Self {
        Self {
            ticks: HashMap::new(),
        }
    }
}

#[derive(Resource, Default)]
pub struct PendingClientJoins(pub HashMap<u128, u64>);

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
