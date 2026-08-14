use bevy::prelude::*;
use serde::{Deserialize, Serialize};
use std::marker::PhantomData;

#[derive(Message, Serialize, Deserialize, Debug, Clone)]
pub struct JoinSnapshotRequest;

#[derive(Message, Serialize, Deserialize, Debug, Clone)]
pub struct JoinSnapshotResponse<S> {
    pub snapshot_tick: u64,
    pub snapshot: S,
}

#[derive(Message, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct ClientLoaded;

#[derive(Message, Serialize, Deserialize, Debug, Clone)]
pub struct ClientScheduledActions<A> {
    pub tick: u64,
    pub actions: Vec<A>,
}

/// The host's ruling on one tick: this is what everybody did, now simulate it.
///
/// # One message per simulated tick, empty or not
///
/// `players_actions` being empty means *nobody acted on this tick*, not *this tick is
/// incomplete*. The host only broadcasts a tick it has already simulated, so by the time this
/// goes out there is nothing outstanding for it.
///
/// That distinction is the whole contract, because the client keys its pause on *having
/// received tick N* — and a message it cannot tell apart from one that never arrived would
/// stop the session for ever rather than visibly break it. So a receiver must record the tick
/// itself, not merely the entries in it; [`apply_authoritative_tick`] does that with an
/// `entry(tick).or_default()` before it looks at anybody's actions.
///
/// [`apply_authoritative_tick`]: crate::apply_authoritative_tick
#[derive(Message, Serialize, Deserialize, Debug, Clone)]
pub struct AuthoritativeTick<A> {
    pub tick: u64,
    pub players_actions: Vec<(u128, Vec<A>)>,
}

#[derive(Message, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct ParticipantJoined {
    pub player_uuid: u128,
    pub joined_at_tick: u64,
}

#[derive(Message, Debug, Clone)]
pub struct CaptureJoinSnapshot<S> {
    pub requester: u128,
    pub snapshot_tick: u64,
    pub marker: PhantomData<fn() -> S>,
}

#[derive(Message, Debug, Clone)]
pub struct ProvideJoinSnapshot<S> {
    pub requester: u128,
    pub snapshot_tick: u64,
    pub snapshot: S,
}

#[derive(Message, Debug, Clone)]
pub struct ApplyJoinSnapshot<S> {
    pub snapshot_tick: u64,
    pub snapshot: S,
}

#[derive(Message, Debug, Clone)]
pub struct JoinSnapshotApplied<S> {
    pub snapshot_tick: u64,
    pub marker: PhantomData<fn() -> S>,
}

impl<S> JoinSnapshotApplied<S> {
    pub fn new(snapshot_tick: u64) -> Self {
        Self {
            snapshot_tick,
            marker: PhantomData,
        }
    }
}
