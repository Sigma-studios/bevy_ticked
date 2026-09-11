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

/// A client has applied its join snapshot and is simulating from it.
///
/// Carries the client's tick buffer because the host sizes the joiner's grace window from it. A
/// joiner schedules its actions `client_tick_buffer` ticks ahead of its own clock, and the host
/// used to size the window from *its own* buffer alone — so a client whose adaptive buffer had
/// grown to forty on a satellite link, joining a LAN host whose buffer was four, had its first
/// scheduled tick land well past the first tick the host required of it, and the host waited on
/// the ticks in between for ever.
#[derive(Message, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct ClientLoaded {
    /// The sender's `LockstepConfig::client_tick_buffer` at the moment it loaded.
    pub buffer: u64,
}

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
    /// What the session itself did on this tick: a participant joined or left, the session
    /// paused or resumed. Applied inside the tick on every peer, so every peer sees the roster
    /// change on the same tick — the leave tick the example never had.
    #[serde(default)]
    pub system: Vec<SystemAction>,
    /// Each client's input-arrival margin as the host measured it: how many ticks ahead of the
    /// host's clock that client's newest batch arrived (negative is late). A client sizes its
    /// buffer from its own entry.
    #[serde(default)]
    pub margins: Vec<(u128, i16)>,
}

impl<A> AuthoritativeTick<A> {
    /// A tick with nothing but players' actions.
    pub fn new(tick: u64, players_actions: Vec<(u128, Vec<A>)>) -> Self {
        Self {
            tick,
            players_actions,
            system: Vec::new(),
            margins: Vec::new(),
        }
    }
}

/// Something the session did on a tick, ruled on by the host like an action.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum SystemAction {
    /// This player's actions are part of the simulation from this tick on.
    ParticipantJoined(u128),
    /// This player is gone from this tick on: kicked, disconnected, or left.
    ParticipantLeft(u128),
    /// The session is paused after this tick.
    Pause(LockstepPauseReason),
    /// The session runs again from this tick.
    Resume,
}

/// Why a lockstep session paused.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockstepPauseReason {
    /// The host asked.
    Host,
    /// A participant stopped answering and the session waited for them.
    PeerAway(u128),
    /// A game's own reason.
    Custom(u8),
}

/// Ask the host to pause the session after the tick about to run. Host-side only: a client
/// writing it is ignored (the client-server crate's request path does not exist here yet).
#[derive(Message, Clone, Copy, Debug, PartialEq, Eq)]
pub struct PauseLockstep(pub LockstepPauseReason);

/// Ask the host to resume.
#[derive(Message, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ResumeLockstep;

/// A roster change, written inside the tick it happened on, on every peer. Read it with
/// `TickedEventReader<RosterChange>` to spawn or despawn a player's body on the same tick
/// everywhere.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RosterChange {
    Joined(u128),
    Left(u128),
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

/// Local, never on the wire: a join snapshot has arrived and is about to replace this peer's
/// world.
///
/// The untyped twin of [`ApplyJoinSnapshot`], for the parts of this crate that do not know the
/// game's snapshot type and still have to act on the world being swapped out — the checksum
/// exchange, which must forget every hash it took of the world that is going. It is written in
/// the same frame and before [`LockstepJoinSet::ApplyJoinSnapshot`], so a reader ordered after
/// that set sees it before the first tick of the new world is sampled.
///
/// [`LockstepJoinSet::ApplyJoinSnapshot`]: crate::LockstepJoinSet::ApplyJoinSnapshot
#[derive(Message, Debug, Clone, Copy, PartialEq, Eq)]
pub struct JoinSnapshotReceived {
    pub snapshot_tick: u64,
}

/// Local, never on the wire: the host accepted a [`ClientLoaded`] and made its sender a
/// participant.
///
/// Three systems used to read `ClientLoaded` off the wire independently — one to activate the
/// participant, one to send it the roster, one to send it the ticks it missed — and each of them
/// re-ran for every copy a client sent. A second `ClientLoaded` from an established participant
/// re-issued its `joined_at_tick`, which reopened its grace window, and re-sent a catch-up whose
/// ticks the client had already simulated. Now one system decides, once, and the others act on
/// its decision.
#[derive(Message, Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClientAccepted {
    pub player_uuid: u128,
}
