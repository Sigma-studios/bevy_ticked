//! A lockstep match that outlives its host.
//!
//! # Why lockstep can
//!
//! A snapshot session cannot survive its host: the authoritative world was the host's, and a
//! client only ever held what it was sent. A lockstep session is different in kind. Every peer
//! simulates every tick from the same rulings, so every survivor already *has* the world. What
//! the host alone had was the right to rule the next tick — and, in the moment it went, the
//! rulings it had sent to some survivors and not yet to others.
//!
//! So a new host needs two things and no more: to agree with the survivors on the last tick the
//! old host ruled, and to rule from the one after it. Nobody rewinds.
//!
//! # How
//!
//! When `bevy_ensemble` names the new host ([`HostChanged`]), every peer holds its clock
//! ([`TickHoldReason::HostMigration`]). Each survivor, once it has reached the new host, sends
//! every ruling it holds and a [`MigrationReport`] of where it is. The new host takes the
//! furthest ruled tick it can assemble without a gap — its own rulings, then the survivors' for
//! the ticks it lacks, at most [`HostMigrationPolicy::trust_window`] past its own — and calls it
//! `T`. It sends each survivor the rulings it is missing up to `T` and a [`MigrationResume`];
//! every survivor cuts whatever it held past `T`, sends again the actions it had scheduled for
//! the ticks after it, and the session runs. The old host, and anyone who never reported, leaves
//! on `T + 1`, on every peer.
//!
//! # What is trusted
//!
//! A ruling the new host did not hold itself comes from a survivor's copy of the old host's
//! word, which the new host cannot check. It is bounded to the trust window, and a copy that
//! differs from the ruling another peer simulated is a divergence the checksum exchange reports.
//! That is the trade the design makes against rewinding: no peer ever simulates a tick twice.
//!
//! # What does not survive
//!
//! A peer that had not finished joining starts its join again from a snapshot. A survivor
//! that reports after `T` was decided, or that had simulated past anything the new host could
//! assemble, leaves on `T + 1` and joins again. A new host that had not finished joining itself
//! cannot rule a world it does not have, and leaves the lobby so that another member is named.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use bevy::prelude::*;
use bevy_ensemble::{
    AwaitingHost, HandshakeVerified, Host, HostChanged, LeaveLobby, Lobby, LobbyClient,
    LobbyClientMessage, LobbyClientPlayerUuid, LobbyMessage, LobbyParticipant, LobbyParticipantOf,
    LocalMultiplayerPlayerId, ReceivedEnsembleMessage, VerifiedHost,
};
use bevy_ticked::tick::{CurrentTick, TickHoldReason, TickHolds};
use serde::{Deserialize, Serialize};

use crate::session::AnnouncedJoins;
use crate::{
    ActionTracker, AdaptiveBufferState, ArrivalMargins, AuthoritativeTick, ClientScheduledActions,
    ClientSnapshotState, JoinSnapshot, JoinSnapshotRequest, LastBroadcastTick, LastScheduledTick,
    LockstepAction, LockstepConfig, LockstepLobbyParticipant, LockstepRoster, LockstepStall,
    OwnInputMargin, PendingLockstepParticipantJoins, PendingSystemActions,
    StashedAuthoritativeTicks, SystemAction, apply_authoritative_tick, insert_actions_into_tracker,
};

/// How far a new host trusts what survivors hold, and how long it waits for them.
#[derive(Resource, Clone, Copy, Debug, PartialEq, Eq)]
pub struct HostMigrationPolicy {
    /// The furthest past its own newest ruling a new host will take a survivor's copy of the old
    /// host's, in ticks; and how many simulated rulings every client keeps, so that a survivor
    /// behind the new host can be filled in.
    ///
    /// Honest survivors are never further apart than a client's buffer and a tick: the old host
    /// rules a tick only once every client's actions for it are in, and a client schedules at
    /// most its buffer ahead of the rulings it holds. The default, 128, is above the largest
    /// buffer the adaptive tuner sizes.
    pub trust_window: u64,
    /// How long a new host waits for every survivor to report before deciding without the ones
    /// that have not. Normally never reached: a survivor that cannot reach the new host is
    /// dropped from the lobby by `bevy_ensemble` first.
    pub report_timeout: Duration,
}

impl Default for HostMigrationPolicy {
    fn default() -> Self {
        Self {
            trust_window: 128,
            report_timeout: Duration::from_secs(30),
        }
    }
}

/// Where this peer is in a host migration. Read it for a status line; it is this crate's to
/// write.
#[derive(Resource, Clone, Debug, Default, PartialEq, Eq)]
pub enum LockstepMigration {
    #[default]
    Idle,
    /// A survivor: telling the new host what it holds, then waiting to be told where to resume.
    Reporting {
        previous_host: u128,
        new_host: u128,
        reported: bool,
    },
    /// The new host: hearing from the survivors.
    Collecting {
        previous_host: u128,
        waited: Duration,
    },
    /// Every peer, after the decision: simulating the ticks up to `resume_after`, which were
    /// already ruled, before the session goes on as before.
    Resuming {
        previous_host: u128,
        resume_after: u64,
    },
}

impl LockstepMigration {
    pub fn is_idle(&self) -> bool {
        matches!(self, Self::Idle)
    }

    /// While the survivors have not agreed where to resume, nobody simulates.
    pub(crate) fn holds_the_clock(&self) -> bool {
        matches!(self, Self::Reporting { .. } | Self::Collecting { .. })
    }

    /// A new host that has not decided `T` yet must not rule anything.
    pub(crate) fn is_collecting(&self) -> bool {
        matches!(self, Self::Collecting { .. })
    }
}

/// Client side: actions this peer scheduled that no ruling has included yet, by tick.
///
/// Kept because a batch sent to a host that is gone is gone with it, and the new host must be
/// sent it again. Forgotten once a ruling for its tick arrives.
#[derive(Resource)]
pub struct UnruledLocalActions<A>(pub BTreeMap<u64, Vec<A>>);

impl<A> Default for UnruledLocalActions<A> {
    fn default() -> Self {
        Self(BTreeMap::new())
    }
}

/// The last migration this peer took part in, so a report that arrives after it was decided can
/// still be answered.
#[derive(Resource, Clone, Copy, Debug, PartialEq, Eq)]
pub struct LastMigration {
    pub previous_host: u128,
    pub resume_after: u64,
}

/// Host side: join snapshot requests that arrived while a migration was under way, to be served
/// once it is over. A snapshot of a world that is still catching up to the ruled ticks would be
/// followed by a catch-up that stops short of them.
#[derive(Resource, Default, Debug)]
pub struct DeferredJoinSnapshotRequests(pub Vec<u128>);

/// New host side: what the survivors have said.
#[derive(Resource)]
pub(crate) struct MigrationCollection<A> {
    reports: BTreeMap<u128, MigrationReport>,
    rulings: BTreeMap<u64, AuthoritativeTick<A>>,
}

impl<A> Default for MigrationCollection<A> {
    fn default() -> Self {
        Self {
            reports: BTreeMap::new(),
            rulings: BTreeMap::new(),
        }
    }
}

/// Client side: the new host's verdict, waiting for the rulings it refers to.
#[derive(Resource, Clone, Copy, Debug)]
pub(crate) struct ParkedVerdict(MigrationResume);

// ── on the wire ──────────────────────────────────────────────────────────────

/// A survivor to its new host: where it is.
#[derive(Message, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub struct MigrationReport {
    /// The host that was lost, so a report cannot be read into a different migration.
    pub previous_host: u128,
    pub current_tick: u64,
    /// The newest tick this survivor holds a ruling for.
    pub newest_ruled: u64,
}

/// A survivor to its new host: one ruling it holds from the old host. Sent for every ruling it
/// holds, before its [`MigrationReport`].
#[derive(Message, Serialize, Deserialize, Debug, Clone)]
pub struct MigrationRuling<A> {
    pub previous_host: u128,
    pub ruling: AuthoritativeTick<A>,
}

/// The new host to a survivor: the old host's rulings end at `resume_after`.
#[derive(Message, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub struct MigrationResume {
    pub previous_host: u128,
    pub resume_after: u64,
    pub verdict: ResumeVerdict,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResumeVerdict {
    /// Simulate up to `resume_after` from the rulings held, and on from there.
    Continue,
    /// This peer leaves the simulation on `resume_after + 1` and must join again from a snapshot.
    Rejoin,
}

// ── local messages ───────────────────────────────────────────────────────────

/// A migration this peer took part in was decided.
#[derive(Message, Debug, Clone, Copy, PartialEq, Eq)]
pub struct LockstepResumed {
    pub previous_host: u128,
    pub resume_after: u64,
    pub verdict: ResumeVerdict,
}

/// This peer was named host and cannot be: it had not finished joining. It leaves the lobby.
#[derive(Message, Debug, Clone, PartialEq, Eq)]
pub struct LockstepMigrationFailed {
    pub reason: String,
}

/// Ordering against a migration, for a game that reads [`LockstepResumed`] in `PreUpdate`.
#[derive(SystemSet, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LockstepMigrationSet {
    /// A host change is noticed and the clock held.
    Begin,
    /// A client applies the new host's verdict.
    ApplyVerdict,
}

// ── every peer ───────────────────────────────────────────────────────────────

/// Is this peer established in the session: loaded, and on the roster the host keeps?
fn established<S: JoinSnapshot>(world: &mut World, lobby: Entity, me: u128) -> bool {
    if !world.resource::<ClientSnapshotState<S>>().ready {
        return false;
    }
    world
        .query::<(
            &LobbyParticipant,
            &LobbyParticipantOf,
            Has<LockstepLobbyParticipant>,
        )>()
        .iter(world)
        .any(|(participant, of, lockstep)| {
            of.0 == lobby && participant.player_uuid == me && lockstep
        })
}

/// The lobby's host changed: hold, and start reporting or collecting.
pub(crate) fn begin_migration<A: LockstepAction, S: JoinSnapshot>(
    world: &mut World,
    mut cursor: Local<bevy::ecs::message::MessageCursor<HostChanged>>,
) {
    let Some(change) = cursor
        .read(world.resource::<Messages<HostChanged>>())
        .last()
        .cloned()
    else {
        return;
    };
    let Some(me) = world
        .get_resource::<LocalMultiplayerPlayerId>()
        .map(|me| me.0)
    else {
        return;
    };

    // Measurements of a link to a peer that is gone.
    world.resource_mut::<OwnInputMargin>().0 = None;
    world.resource_mut::<ArrivalMargins>().0.clear();
    *world.resource_mut::<LockstepStall>() = LockstepStall::default();
    if let Some(mut adaptive) = world.get_resource_mut::<AdaptiveBufferState>() {
        *adaptive = AdaptiveBufferState::default();
    }

    let state = if change.promoted {
        if !established::<S>(world, change.lobby, me) {
            let reason = "named host before this peer had finished joining the session".to_owned();
            warn!("stepping down: {reason}");
            world.write_message(LockstepMigrationFailed { reason });
            world.write_message(LeaveLobby);
            LockstepMigration::Idle
        } else {
            let current = world.resource::<CurrentTick>().0;
            let newest = world
                .resource::<ActionTracker<A>>()
                .newest_tick()
                .unwrap_or(current)
                .max(current);
            // Nothing past what this peer already holds is ruled by it until the survivors have
            // been heard; nothing before is ruled again.
            world.resource_mut::<LastBroadcastTick>().0 = newest;
            *world.resource_mut::<MigrationCollection<A>>() = MigrationCollection::default();
            info!(
                "this peer hosts the lockstep session now, in place of {:#x}; it holds rulings \
                 up to tick {newest} and is waiting for the survivors",
                change.previous
            );
            LockstepMigration::Collecting {
                previous_host: change.previous,
                waited: Duration::ZERO,
            }
        }
    } else {
        LockstepMigration::Reporting {
            previous_host: change.previous,
            new_host: change.new,
            reported: false,
        }
    };
    *world.resource_mut::<LockstepMigration>() = state;
}

/// The clock is held while this peer waits for a host, and while the survivors agree where to
/// resume. Decided every frame, so nothing has to remember to release it.
pub(crate) fn hold_during_migration(
    migration: Res<LockstepMigration>,
    waiting: Query<(), (With<Lobby>, With<AwaitingHost>)>,
    mut holds: ResMut<TickHolds>,
) {
    let held = migration.holds_the_clock() || !waiting.is_empty();
    if holds.holds(TickHoldReason::HostMigration) != held {
        holds.set(TickHoldReason::HostMigration, held);
    }
}

/// A session that ends takes its migration with it.
pub(crate) fn forget_migration_on_lobby_removed<A: LockstepAction>(
    mut removed: RemovedComponents<Lobby>,
    mut migration: ResMut<LockstepMigration>,
    mut collection: ResMut<MigrationCollection<A>>,
    mut unruled: ResMut<UnruledLocalActions<A>>,
    mut deferred: ResMut<DeferredJoinSnapshotRequests>,
    mut holds: ResMut<TickHolds>,
    mut commands: Commands,
) {
    if removed.read().next().is_none() {
        return;
    }
    *migration = LockstepMigration::Idle;
    *collection = MigrationCollection::default();
    unruled.0.clear();
    deferred.0.clear();
    holds.release(TickHoldReason::HostMigration);
    commands.remove_resource::<LastMigration>();
    commands.remove_resource::<ParkedVerdict>();
}

/// Once every ruled tick up to `resume_after` has been simulated, the session is as it was.
pub(crate) fn end_resume(mut migration: ResMut<LockstepMigration>, tick: Res<CurrentTick>) {
    if let LockstepMigration::Resuming { resume_after, .. } = *migration
        && tick.0 >= resume_after
    {
        *migration = LockstepMigration::Idle;
    }
}

/// Start this peer's join over: it has no world the new host can resume.
fn restart_join<A: LockstepAction, S: JoinSnapshot>(world: &mut World, lobby: Entity) {
    world.resource_mut::<ClientSnapshotState<S>>().ready = false;
    {
        let mut tracker = world.resource_mut::<ActionTracker<A>>();
        tracker.ticks.clear();
        tracker.system.clear();
    }
    world
        .resource_mut::<StashedAuthoritativeTicks<A>>()
        .0
        .clear();
    world.resource_mut::<UnruledLocalActions<A>>().0.clear();
    world.resource_mut::<LastScheduledTick>().0 = None;
    world.resource_mut::<LockstepRoster>().0.clear();
    world
        .resource_mut::<PendingLockstepParticipantJoins>()
        .0
        .clear();
    world.trigger(LobbyMessage::new(lobby, JoinSnapshotRequest));
}

// ── a survivor ───────────────────────────────────────────────────────────────

/// Once the new host is reached, tell it everything this peer holds — or, if this peer had not
/// finished joining, start the join again with it.
pub(crate) fn report_to_new_host<A: LockstepAction, S: JoinSnapshot>(world: &mut World) {
    let LockstepMigration::Reporting {
        previous_host,
        new_host,
        reported: false,
    } = *world.resource::<LockstepMigration>()
    else {
        return;
    };
    let Some(me) = world
        .get_resource::<LocalMultiplayerPlayerId>()
        .map(|me| me.0)
    else {
        return;
    };
    let reached = world
        .query_filtered::<(Entity, &VerifiedHost), (With<Lobby>, Without<Host>, With<HandshakeVerified>)>()
        .iter(world)
        .find(|(_, verified)| verified.0 == new_host)
        .map(|(lobby, _)| lobby);
    let Some(lobby) = reached else {
        return;
    };

    if !established::<S>(world, lobby, me) {
        info!("had not finished joining when the host changed; joining the new host afresh");
        restart_join::<A, S>(world, lobby);
        *world.resource_mut::<LockstepMigration>() = LockstepMigration::Idle;
        return;
    }

    let current = world.resource::<CurrentTick>().0;
    let rulings: Vec<AuthoritativeTick<A>> = {
        let tracker = world.resource::<ActionTracker<A>>();
        let mut ticks: Vec<u64> = tracker.ticks.keys().copied().collect();
        ticks.sort_unstable();
        ticks
            .into_iter()
            .map(|tick| ruling_from(tracker, tick))
            .collect()
    };
    let newest_ruled = rulings
        .last()
        .map_or(current, |ruling| ruling.tick)
        .max(current);
    let held = rulings.len();
    for ruling in rulings {
        world.trigger(LobbyMessage::new(
            lobby,
            MigrationRuling {
                previous_host,
                ruling,
            },
        ));
    }
    world.trigger(LobbyMessage::new(
        lobby,
        MigrationReport {
            previous_host,
            current_tick: current,
            newest_ruled,
        },
    ));
    info!(
        "reported to the new host {new_host:#x}: at tick {current}, {held} rulings held up to \
         {newest_ruled}"
    );
    *world.resource_mut::<LockstepMigration>() = LockstepMigration::Reporting {
        previous_host,
        new_host,
        reported: true,
    };
}

/// The ruling for `tick` as this peer holds it, in the shape it travels in.
fn ruling_from<A: LockstepAction>(tracker: &ActionTracker<A>, tick: u64) -> AuthoritativeTick<A> {
    AuthoritativeTick {
        tick,
        players_actions: tracker
            .ticks
            .get(&tick)
            .map(|players| {
                players
                    .iter()
                    .map(|(uuid, actions)| (*uuid, actions.clone()))
                    .collect()
            })
            .unwrap_or_default(),
        system: tracker.system_actions_for_tick(tick).to_vec(),
        margins: Vec::new(),
    }
}

/// Apply the new host's verdict, once every ruling it refers to is here.
pub(crate) fn apply_migration_verdict<A: LockstepAction, S: JoinSnapshot>(
    world: &mut World,
    mut cursor: Local<bevy::ecs::message::MessageCursor<ReceivedEnsembleMessage<MigrationResume>>>,
) {
    let arrived = cursor
        .read(world.resource::<Messages<ReceivedEnsembleMessage<MigrationResume>>>())
        .last()
        .map(|received| received.message);
    if let Some(verdict) = arrived {
        world.insert_resource(ParkedVerdict(verdict));
    }
    let Some(ParkedVerdict(verdict)) = world.get_resource::<ParkedVerdict>().copied() else {
        return;
    };
    let LockstepMigration::Reporting { previous_host, .. } = *world.resource::<LockstepMigration>()
    else {
        world.remove_resource::<ParkedVerdict>();
        return;
    };
    if verdict.previous_host != previous_host {
        // About a migration this peer is no longer in.
        world.remove_resource::<ParkedVerdict>();
        return;
    }
    let Some(lobby) = world
        .query_filtered::<Entity, (With<Lobby>, Without<Host>)>()
        .iter(world)
        .next()
    else {
        return;
    };
    let resume_after = verdict.resume_after;

    match verdict.verdict {
        ResumeVerdict::Rejoin => {
            warn!(
                "the new host resumed the session at tick {resume_after} without this peer; \
                 joining it again"
            );
            world.remove_resource::<ParkedVerdict>();
            restart_join::<A, S>(world, lobby);
            *world.resource_mut::<LockstepMigration>() = LockstepMigration::Idle;
        }
        ResumeVerdict::Continue => {
            let current = world.resource::<CurrentTick>().0;
            let complete = {
                let tracker = world.resource::<ActionTracker<A>>();
                (current + 1..=resume_after).all(|tick| tracker.ticks.contains_key(&tick))
            };
            if !complete {
                return;
            }
            world.remove_resource::<ParkedVerdict>();
            {
                // What the old host ruled past `resume_after` never happened.
                let mut tracker = world.resource_mut::<ActionTracker<A>>();
                tracker.ticks.retain(|tick, _| *tick <= resume_after);
                tracker.system.retain(|tick, _| *tick <= resume_after);
            }
            let unruled = std::mem::take(&mut world.resource_mut::<UnruledLocalActions<A>>().0);
            let buffer = world.resource::<LockstepConfig>().client_tick_buffer;
            let scheduled_through = world
                .resource::<LastScheduledTick>()
                .0
                .unwrap_or(0)
                .max(resume_after + 1 + buffer);
            // Everything this peer had said about the ticks after `resume_after` went to a host
            // that is gone. Said again, every tick of it: the new host waits for each.
            for tick in resume_after + 1..=scheduled_through {
                let actions = unruled.get(&tick).cloned().unwrap_or_default();
                world.trigger(LobbyMessage::new_no_delay(
                    lobby,
                    ClientScheduledActions { tick, actions },
                ));
            }
            world.resource_mut::<UnruledLocalActions<A>>().0 = unruled
                .into_iter()
                .filter(|(tick, _)| *tick > resume_after)
                .collect();
            world.resource_mut::<LastScheduledTick>().0 = Some(scheduled_through);
            info!("resuming the session from tick {resume_after} with the new host");
            *world.resource_mut::<LockstepMigration>() = LockstepMigration::Resuming {
                previous_host,
                resume_after,
            };
        }
    }
    world.insert_resource(LastMigration {
        previous_host,
        resume_after,
    });
    world.write_message(LockstepResumed {
        previous_host,
        resume_after,
        verdict: verdict.verdict,
    });
}

// ── the new host ─────────────────────────────────────────────────────────────

/// Hear the survivors: their rulings while collecting, and a late report with a rejoin.
pub(crate) fn collect_migration_reports<A: LockstepAction>(
    world: &mut World,
    mut reports_cursor: Local<
        bevy::ecs::message::MessageCursor<ReceivedEnsembleMessage<MigrationReport>>,
    >,
    mut rulings_cursor: Local<
        bevy::ecs::message::MessageCursor<ReceivedEnsembleMessage<MigrationRuling<A>>>,
    >,
) {
    let reports: Vec<(Option<u128>, MigrationReport)> = reports_cursor
        .read(world.resource::<Messages<ReceivedEnsembleMessage<MigrationReport>>>())
        .map(|received| (received.sender, received.message))
        .collect();
    let rulings: Vec<(Option<u128>, MigrationRuling<A>)> = rulings_cursor
        .read(world.resource::<Messages<ReceivedEnsembleMessage<MigrationRuling<A>>>>())
        .map(|received| (received.sender, received.message.clone()))
        .collect();
    if reports.is_empty() && rulings.is_empty() {
        return;
    }
    let Some(host_lobby) = world
        .query_filtered::<Entity, (With<Lobby>, With<Host>)>()
        .iter(world)
        .next()
    else {
        return;
    };
    let participants: BTreeSet<u128> = world
        .query_filtered::<(&LobbyParticipant, &LobbyParticipantOf), With<LockstepLobbyParticipant>>(
        )
        .iter(world)
        .filter(|(_, of)| of.0 == host_lobby)
        .map(|(participant, _)| participant.player_uuid)
        .collect();

    match world.resource::<LockstepMigration>().clone() {
        LockstepMigration::Collecting { previous_host, .. } => {
            let mut collection = world.resource_mut::<MigrationCollection<A>>();
            for (sender, ruling) in rulings {
                let Some(sender) = sender else { continue };
                if ruling.previous_host != previous_host || !participants.contains(&sender) {
                    continue;
                }
                collection
                    .rulings
                    .entry(ruling.ruling.tick)
                    .or_insert(ruling.ruling);
            }
            for (sender, report) in reports {
                let Some(sender) = sender else { continue };
                if report.previous_host != previous_host || !participants.contains(&sender) {
                    continue;
                }
                collection.reports.insert(sender, report);
            }
        }
        _ => {
            let Some(last) = world.get_resource::<LastMigration>().copied() else {
                return;
            };
            for (sender, report) in reports {
                let Some(sender) = sender else { continue };
                if report.previous_host != last.previous_host {
                    continue;
                }
                warn!(
                    "{sender:#x} reported after the session had resumed at tick {}; it joins again",
                    last.resume_after
                );
                if let Some(seat) = seat_of(world, sender) {
                    world.trigger(LobbyClientMessage::new(
                        seat,
                        MigrationResume {
                            previous_host: last.previous_host,
                            resume_after: last.resume_after,
                            verdict: ResumeVerdict::Rejoin,
                        },
                    ));
                }
            }
        }
    }
}

fn seat_of(world: &mut World, uuid: u128) -> Option<Entity> {
    world
        .query_filtered::<(Entity, &LobbyClientPlayerUuid), With<LobbyClient>>()
        .iter(world)
        .find(|(_, seat)| seat.0 == uuid)
        .map(|(seat, _)| seat)
}

/// Once every survivor has reported, or the wait is over: decide where the old host's rulings
/// end, bring every survivor up to it, and rule on from there.
pub(crate) fn decide_resume_tick<A: LockstepAction>(world: &mut World) {
    let LockstepMigration::Collecting {
        previous_host,
        waited,
    } = *world.resource::<LockstepMigration>()
    else {
        return;
    };
    let waited = waited + world.resource::<Time>().delta();
    *world.resource_mut::<LockstepMigration>() = LockstepMigration::Collecting {
        previous_host,
        waited,
    };
    let Some(me) = world
        .get_resource::<LocalMultiplayerPlayerId>()
        .map(|me| me.0)
    else {
        return;
    };
    let Some(host_lobby) = world
        .query_filtered::<Entity, (With<Lobby>, With<Host>)>()
        .iter(world)
        .next()
    else {
        return;
    };
    let policy = world
        .get_resource::<HostMigrationPolicy>()
        .copied()
        .unwrap_or_default();

    // Everyone in the simulation but this peer: whose word the decision waits for.
    let participants: BTreeMap<u128, (Entity, u64)> = world
        .query::<(
            Entity,
            &LobbyParticipant,
            &LobbyParticipantOf,
            &LockstepLobbyParticipant,
        )>()
        .iter(world)
        .filter(|(_, participant, of, _)| of.0 == host_lobby && participant.player_uuid != me)
        .map(|(entity, participant, _, lockstep)| {
            (participant.player_uuid, (entity, lockstep.joined_at_tick))
        })
        .collect();
    let all_reported = {
        let collection = world.resource::<MigrationCollection<A>>();
        participants
            .keys()
            .all(|uuid| collection.reports.contains_key(uuid))
    };
    if !all_reported && waited < policy.report_timeout {
        return;
    }

    let MigrationCollection { reports, rulings } =
        std::mem::take(&mut *world.resource_mut::<MigrationCollection<A>>());
    let current = world.resource::<CurrentTick>().0;

    // The furthest ruled tick that can be assembled without a gap: this peer's own rulings
    // first, a survivor's copy for a tick this peer lacks, and never past the trust window.
    let resume_after = {
        let mut tracker = world.resource_mut::<ActionTracker<A>>();
        let own_newest = tracker.newest_tick().unwrap_or(current).max(current);
        let mut resume_after = own_newest;
        while resume_after < own_newest + policy.trust_window {
            let next = resume_after + 1;
            if !tracker.ticks.contains_key(&next) {
                let Some(ruling) = rulings.get(&next) else {
                    break;
                };
                apply_authoritative_tick(&mut tracker, ruling);
            }
            resume_after = next;
        }
        resume_after
    };
    world.resource_mut::<LastBroadcastTick>().0 = resume_after;

    // Who is in the simulation as of `resume_after`.
    let roster_at_resume: BTreeSet<u128> = {
        let tracker = world.resource::<ActionTracker<A>>();
        let mut roster = world.resource::<LockstepRoster>().0.clone();
        for tick in current + 1..=resume_after {
            for action in tracker.system_actions_for_tick(tick) {
                match *action {
                    SystemAction::ParticipantJoined(uuid) => {
                        roster.insert(uuid);
                    }
                    SystemAction::ParticipantLeft(uuid) => {
                        roster.remove(&uuid);
                    }
                    SystemAction::Pause(_) | SystemAction::Resume => {}
                }
            }
        }
        roster
    };

    let seats: BTreeMap<u128, Entity> = world
        .query_filtered::<(Entity, &LobbyClientPlayerUuid), With<LobbyClient>>()
        .iter(world)
        .map(|(seat, uuid)| (uuid.0, seat))
        .collect();
    let mut continuing = BTreeSet::from([me]);
    let mut verdicts: Vec<(Entity, Vec<AuthoritativeTick<A>>, MigrationResume)> = Vec::new();
    let tracker = world.resource::<ActionTracker<A>>();
    for (uuid, report) in &reports {
        let Some(&seat) = seats.get(uuid) else {
            continue;
        };
        let missing = report.newest_ruled.max(report.current_tick) + 1..=resume_after;
        let fillable = missing
            .clone()
            .all(|tick| tracker.ticks.contains_key(&tick));
        let joins_later = participants
            .get(uuid)
            .is_some_and(|(_, joined_at)| *joined_at > resume_after);
        let continues = report.current_tick <= resume_after
            && fillable
            && (roster_at_resume.contains(uuid) || joins_later);
        let verdict = MigrationResume {
            previous_host,
            resume_after,
            verdict: if continues {
                ResumeVerdict::Continue
            } else {
                ResumeVerdict::Rejoin
            },
        };
        if continues {
            continuing.insert(*uuid);
            let fill = missing.map(|tick| ruling_from(tracker, tick)).collect();
            verdicts.push((seat, fill, verdict));
        } else {
            warn!(
                "{uuid:#x} cannot resume at tick {resume_after} (at tick {}, ruled to {}); it \
                 leaves and joins again",
                report.current_tick, report.newest_ruled
            );
            verdicts.push((seat, Vec::new(), verdict));
        }
    }

    // Everyone in the simulation who is not going on in it leaves on the first tick this peer
    // rules — the old host among them — on every peer alike.
    let leaving: Vec<u128> = roster_at_resume
        .iter()
        .copied()
        .filter(|uuid| !continuing.contains(uuid))
        .collect();
    for uuid in &leaving {
        world
            .resource_mut::<PendingSystemActions>()
            .0
            .push(SystemAction::ParticipantLeft(*uuid));
    }
    // And nobody not going on is required any more, whether or not they had joined yet.
    for (uuid, (entity, _)) in &participants {
        if !continuing.contains(uuid) {
            world
                .entity_mut(*entity)
                .remove::<LockstepLobbyParticipant>();
        }
    }
    world.resource_mut::<AnnouncedJoins>().0 = roster_at_resume
        .iter()
        .copied()
        .filter(|uuid| continuing.contains(uuid))
        .collect();

    // What this peer had scheduled as a client went to the old host; as the host it goes
    // straight into the ticks it rules.
    let unruled = std::mem::take(&mut world.resource_mut::<UnruledLocalActions<A>>().0);
    {
        let mut tracker = world.resource_mut::<ActionTracker<A>>();
        for (tick, actions) in unruled {
            if tick > resume_after {
                insert_actions_into_tracker(&mut tracker, tick, me, actions);
            }
        }
    }
    world.resource_mut::<ArrivalMargins>().0.clear();

    for (seat, fill, verdict) in verdicts {
        for ruling in fill {
            world.trigger(LobbyClientMessage::new(seat, ruling));
        }
        world.trigger(LobbyClientMessage::new(seat, verdict));
    }

    info!(
        "resuming the session at tick {resume_after} (this peer at {current}); {} going on, {} \
         leaving",
        continuing.len(),
        leaving.len()
    );
    *world.resource_mut::<LockstepMigration>() = LockstepMigration::Resuming {
        previous_host,
        resume_after,
    };
    world.insert_resource(LastMigration {
        previous_host,
        resume_after,
    });
    world.write_message(LockstepResumed {
        previous_host,
        resume_after,
        verdict: ResumeVerdict::Continue,
    });
}
