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
