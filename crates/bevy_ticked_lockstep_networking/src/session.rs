//! The session's own actions: who is in it, whether it is paused, who it is waiting on.
//!
//! # A leave tick, at last
//!
//! A participant used to join on an agreed tick (`joined_at_tick`, carried by
//! `ParticipantJoined`) and leave on no tick at all: the host stopped requiring their actions
//! when their `LobbyClient` went, and every peer noticed the departure on whatever frame its
//! roster changed. The example could not despawn a departed body deterministically, so it kept
//! it. Now the host rules a [`SystemAction::ParticipantLeft`] into the next tick it simulates,
//! like an action, and every peer applies it inside that tick: [`LockstepRoster`] changes and a
//! [`RosterChange`] is written on the same tick everywhere.
//!
//! # The stall
//!
//! Lockstep waits. A participant whose actions stop coming stops the session for everybody,
//! and there was nothing to say who, for how long, or what would happen. [`LockstepStall`]
//! says who and for how long; [`StallPolicy`] says what happens: after `pause_after` the stall
//! is reported as a pause, after `kick_after` the host kicks the participant, everybody sees
//! the leave on one tick, and the session runs again.
//!
//! # Catching up
//!
//! A joiner receives every tick it missed in a burst and used to simulate them one per frame,
//! trailing the host by the join's length for the rest of the session while the host froze to
//! wait. It runs its clock up to fifty percent fast until the backlog is gone, and the host
//! never freezes for it.

use std::time::Duration;

use bevy::prelude::*;
use bevy_ensemble::{
    Host, Lobby, LobbyClient, LobbyClientPlayerUuid, LobbyParticipant, LobbyParticipantOf,
};
use bevy_ticked::{
    events::{TickedEventAppExt, TickedEventWriter},
    tick::{CurrentTick, TickHoldReason, TickHolds},
    time::TickRateDilation,
};

use crate::{
    ActionTracker, ArrivalMargins, LockstepAction, LockstepConfig, LockstepLobbyParticipant,
    LockstepPauseReason, LockstepPaused, LockstepRoster, LockstepStall, OwnInputMargin,
    PauseLockstep, PendingSystemActions, ResumeLockstep, RosterChange, StallPolicy, SystemAction,
    participant_is_required_for_tick, tracker_has_actions_for_player,
};

/// Host side: participants whose join has been ruled into a tick, so it is ruled once.
#[derive(Resource, Default, Debug, Clone)]
pub struct AnnouncedJoins(pub std::collections::BTreeSet<u128>);

pub(crate) fn install<A: LockstepAction>(app: &mut App) {
    app.init_resource::<AnnouncedJoins>()
        .init_resource::<PendingSystemActions>()
        .init_resource::<LockstepRoster>()
        .init_resource::<LockstepPaused>()
        .init_resource::<StallPolicy>()
        .init_resource::<LockstepStall>()
        .init_resource::<OwnInputMargin>()
        .init_resource::<ArrivalMargins>()
        .add_message::<PauseLockstep>()
        .add_message::<ResumeLockstep>()
        .add_ticked_event::<RosterChange>()
        .add_observer(queue_departure)
        .add_systems(
            bevy_ticked::TickedLoop,
            (
                stage_system_actions.in_set(bevy_ticked::TickedSystems::PreTick),
                host_hold_when_paused
                    .in_set(bevy_ticked::TickedSystems::PreTick)
                    .after(stage_system_actions),
            ),
        )
        .add_systems(
            bevy_ticked::TickedSimulation,
            apply_system_actions::<A>.in_set(LockstepSimulationSet::System),
        )
        .configure_sets(
            bevy_ticked::TickedSimulation,
            LockstepSimulationSet::System.before(LockstepSimulationSet::Game),
        )
        .add_systems(
            Update,
            (
                reset_on_lobby_removed,
                track_stall::<A>,
                kick_stalled_participants.after(track_stall::<A>),
                catch_up::<A>,
                size_buffer_from_margin,
            ),
        );
}

/// A session that ends takes its roster, its pause and its pending rulings with it.
fn reset_on_lobby_removed(
    mut removed: RemovedComponents<Lobby>,
    mut announced: ResMut<AnnouncedJoins>,
    mut pending: ResMut<PendingSystemActions>,
    mut roster: ResMut<LockstepRoster>,
    mut paused: ResMut<LockstepPaused>,
    mut stall: ResMut<LockstepStall>,
    mut margin: ResMut<OwnInputMargin>,
    mut margins: ResMut<ArrivalMargins>,
    mut commands: Commands,
) {
    if removed.read().next().is_none() {
        return;
    }
    announced.0.clear();
    pending.0.clear();
    roster.0.clear();
    paused.0 = None;
    *stall = LockstepStall::default();
    margin.0 = None;
    margins.0.clear();
    commands.remove_resource::<ResumeStaged>();
}

/// Ordering inside the simulation: the session's actions land before the game's systems.
#[derive(SystemSet, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LockstepSimulationSet {
    /// Roster and pause changes for this tick.
    System,
    /// The game. Put your simulation here to see the roster as of this tick.
    Game,
}

// ── the host rules ───────────────────────────────────────────────────────────

/// A client that is gone leaves the simulation on the next tick, and the host stops waiting
/// on it now.
fn queue_departure(
    removed: On<Remove, LobbyClient>,
    uuids: Query<&LobbyClientPlayerUuid>,
    participants: Query<(Entity, &LobbyParticipant), With<LockstepLobbyParticipant>>,
    mut pending: ResMut<PendingSystemActions>,
    mut announced: ResMut<AnnouncedJoins>,
    mut commands: Commands,
) {
    let Ok(uuid) = uuids.get(removed.entity) else {
        return;
    };
    let Some((entity, _)) = participants
        .iter()
        .find(|(_, participant)| participant.player_uuid == uuid.0)
    else {
        return;
    };
    commands
        .entity(entity)
        .try_remove::<LockstepLobbyParticipant>();
    pending.0.push(SystemAction::ParticipantLeft(uuid.0));
    announced.0.remove(&uuid.0);
}

/// Before the host's tick: whatever the session did since the last one goes into the tick
/// about to run — joins whose agreed tick this is, departures, a pause or a resume.
fn stage_system_actions(world: &mut World) {
    let hosting = {
        let mut hosts = world.query_filtered::<(), (With<Lobby>, With<Host>)>();
        hosts.iter(world).next().is_some()
    };
    if !hosting {
        return;
    }
    let next_tick = world.resource::<CurrentTick>().0 + 1;
    let mut actions = std::mem::take(&mut world.resource_mut::<PendingSystemActions>().0);

    // Everyone whose agreed tick this is, plus anyone whose agreed tick has already passed
    // and who was never announced (the host itself, whose participant predates the first
    // tick).
    let joining: Vec<u128> = {
        let announced = world.resource::<AnnouncedJoins>().0.clone();
        let mut participants = world.query::<(
            &LobbyParticipant,
            &LockstepLobbyParticipant,
            &LobbyParticipantOf,
        )>();
        participants
            .iter(world)
            .filter(|(participant, lockstep, _)| {
                lockstep.joined_at_tick <= next_tick
                    && !announced.contains(&participant.player_uuid)
            })
            .map(|(participant, _, _)| participant.player_uuid)
            .collect()
    };
    for uuid in joining {
        world.resource_mut::<AnnouncedJoins>().0.insert(uuid);
        actions.push(SystemAction::ParticipantJoined(uuid));
    }

    let pauses: Vec<LockstepPauseReason> = world
        .resource_mut::<Messages<PauseLockstep>>()
        .drain()
        .map(|p| p.0)
        .collect();
    let resumes = world
        .resource_mut::<Messages<ResumeLockstep>>()
        .drain()
        .count();
    let paused = world.resource::<LockstepPaused>().0.is_some();
    if let Some(reason) = pauses.last().copied()
        && !paused
    {
        actions.push(SystemAction::Pause(reason));
    } else if resumes > 0 && paused {
        actions.push(SystemAction::Resume);
        world.insert_resource(ResumeStaged);
    }

    if actions.is_empty() {
        return;
    }
    // Typed `A` is not known here; every tracker stores system actions under its own key, so
    // reach the one the plugin registered through a type-erased hook.
    if let Some(stage) = world.get_resource::<StageSystemActions>().copied() {
        (stage.0)(world, next_tick, actions);
    }
}

/// Type-erased "push these system actions into the tracker for `tick`".
#[derive(Resource, Clone, Copy)]
pub(crate) struct StageSystemActions(pub(crate) fn(&mut World, u64, Vec<SystemAction>));

pub(crate) fn stage_into_tracker<A: LockstepAction>(
    world: &mut World,
    tick: u64,
    actions: Vec<SystemAction>,
) {
    world
        .resource_mut::<ActionTracker<A>>()
        .system
        .entry(tick)
        .or_default()
        .extend(actions);
}

/// The host holds after a tick that paused, and runs again from the tick that resumes.
///
/// The paused state is what the simulation applied ([`LockstepPaused`]), so the tick carrying
/// the pause has run before the hold, and the tick carrying the resume is staged before it is
/// lifted: the next pass finds a resume in the tracker for the tick about to run.
fn host_hold_when_paused(
    host: Query<(), (With<Lobby>, With<Host>)>,
    paused: Res<LockstepPaused>,
    stall: Res<LockstepStall>,
    mut holds: ResMut<TickHolds>,
    resume_staged: Option<Res<ResumeStaged>>,
) {
    if host.is_empty() {
        return;
    }
    let _ = stall;
    let held = paused.0.is_some() && resume_staged.is_none();
    holds.set(TickHoldReason::SessionPause, held);
}

/// Present on the host while a resume has been ruled into the next tick and that tick has not
/// yet run.
#[derive(Resource, Debug, Clone, Copy)]
struct ResumeStaged;

// ── every peer applies ───────────────────────────────────────────────────────

/// Inside the tick, first: the session's actions for this tick.
fn apply_system_actions<A: LockstepAction>(
    tick: Res<CurrentTick>,
    tracker: Res<ActionTracker<A>>,
    mut roster: ResMut<LockstepRoster>,
    mut paused: ResMut<LockstepPaused>,
    mut changes: TickedEventWriter<RosterChange>,
    mut commands: Commands,
) {
    for action in tracker.system_actions_for_tick(tick.0) {
        match *action {
            SystemAction::ParticipantJoined(uuid) => {
                if roster.0.insert(uuid) {
                    changes.write(tick.0, RosterChange::Joined(uuid));
                }
            }
            SystemAction::ParticipantLeft(uuid) => {
                if roster.0.remove(&uuid) {
                    changes.write(tick.0, RosterChange::Left(uuid));
                }
            }
            SystemAction::Pause(reason) => {
                paused.0 = Some(reason);
                commands.remove_resource::<ResumeStaged>();
            }
            SystemAction::Resume => {
                paused.0 = None;
                commands.remove_resource::<ResumeStaged>();
            }
        }
    }
}

// ── the stall ────────────────────────────────────────────────────────────────

/// Who this peer is waiting on, on the frame clock.
fn track_stall<A: LockstepAction>(
    time: Res<Time<Real>>,
    holds: Res<TickHolds>,
    tick: Res<CurrentTick>,
    config: Res<LockstepConfig>,
    policy: Res<StallPolicy>,
    tracker: Res<ActionTracker<A>>,
    host: Query<(), (With<Lobby>, With<Host>)>,
    client: Query<(), (With<Lobby>, Without<Host>)>,
    participants: Query<(&LobbyParticipant, &LockstepLobbyParticipant)>,
    mut stall: ResMut<LockstepStall>,
    mut started: Local<std::collections::BTreeMap<u128, Duration>>,
) {
    let waiting = holds.holds(TickHoldReason::WaitingForPeers);
    if !waiting {
        started.clear();
        if !stall.waiting_on.is_empty() || stall.paused {
            *stall = LockstepStall::default();
        }
        return;
    }
    let next = tick.0 + 1;
    let waiting_on: Vec<u128> = if !host.is_empty() {
        participants
            .iter()
            .filter(|(participant, lockstep)| {
                !participant.is_host
                    && participant_is_required_for_tick(lockstep, next)
                    && next > lockstep.joined_at_tick + config.host_tick_buffer
                    && !tracker_has_actions_for_player(&tracker, next, participant.player_uuid)
            })
            .map(|(participant, _)| participant.player_uuid)
            .collect()
    } else if !client.is_empty() {
        participants
            .iter()
            .filter(|(participant, _)| participant.is_host)
            .map(|(participant, _)| participant.player_uuid)
            .collect()
    } else {
        Vec::new()
    };
    // Each peer is timed on its own: a kick that empties the wait must not carry its clock
    // over to whoever the host waits on next for a frame.
    let now = time.elapsed();
    started.retain(|uuid, _| waiting_on.contains(uuid));
    for uuid in &waiting_on {
        started.entry(*uuid).or_insert(now);
    }
    let since = started
        .values()
        .map(|from| now.saturating_sub(*from))
        .max()
        .unwrap_or_default();
    let paused = since >= policy.pause_after;
    let next_stall = LockstepStall {
        waiting_on,
        since,
        paused,
    };
    if *stall != next_stall {
        *stall = next_stall;
    }
}

/// The host kicks whoever it has waited on for too long: the leave lands on one tick for
/// everybody, and the session runs again.
fn kick_stalled_participants(
    policy: Res<StallPolicy>,
    stall: Res<LockstepStall>,
    host: Query<(), (With<Lobby>, With<Host>)>,
    clients: Query<(Entity, &LobbyClientPlayerUuid), With<LobbyClient>>,
    mut commands: Commands,
) {
    if host.is_empty() {
        return;
    }
    let Some(limit) = policy.kick_after else {
        return;
    };
    if stall.since < limit {
        return;
    }
    for uuid in &stall.waiting_on {
        if let Some((entity, _)) = clients.iter().find(|(_, client)| client.0 == *uuid) {
            warn!(
                "kicking {uuid:#x}: the session waited {:.1}s for their actions (limit {:.1}s)",
                stall.since.as_secs_f64(),
                limit.as_secs_f64()
            );
            // The `LobbyClient` going is what `queue_departure` watches; the transport tells
            // the client it was kicked.
            commands.entity(entity).try_despawn();
        }
    }
}

// ── catching up, and the buffer ──────────────────────────────────────────────

/// Largest speed-up while a joiner works through its backlog.
const MAX_CATCH_UP: f64 = 0.5;

/// A client with more ticks in hand than its buffer runs fast until it has caught up.
fn catch_up<A: LockstepAction>(
    tick: Res<CurrentTick>,
    config: Res<LockstepConfig>,
    tracker: Res<ActionTracker<A>>,
    client: Query<(), (With<Lobby>, Without<Host>)>,
    dilation: Option<ResMut<TickRateDilation>>,
) {
    let Some(mut dilation) = dilation else {
        return;
    };
    if client.is_empty() {
        return;
    }
    let backlog = tracker
        .newest_tick()
        .map_or(0, |newest| newest.saturating_sub(tick.0));
    let target = if backlog > config.client_tick_buffer + 2 {
        1.0 + (backlog as f64 / 64.0).min(MAX_CATCH_UP)
    } else {
        1.0
    };
    if (dilation.0 - target).abs() > 1e-9 {
        dilation.0 = target;
    }
}

/// The margin a client aims for: its batches arriving this many ticks before the host needs
/// them. Two: one for a frame of jitter, one for the tick.
pub const TARGET_ARRIVAL_MARGIN: i16 = 2;

/// Frames a shrinking buffer waits between steps.
const SHRINK_EVERY_FRAMES: u32 = 128;

/// A client sizes its buffer from its own arrival margin: the host's word on whether its
/// actions arrive in time, which the ping round trip only estimates.
fn size_buffer_from_margin(
    margin: Res<OwnInputMargin>,
    mut config: ResMut<LockstepConfig>,
    client: Query<(), (With<Lobby>, Without<Host>)>,
    tuning: Option<Res<crate::AdaptiveBufferTuning>>,
    mut frames_over: Local<u32>,
) {
    if client.is_empty() || !margin.is_changed() {
        return;
    }
    let Some(margin) = margin.0 else {
        return;
    };
    let floor = tuning.as_ref().map_or(1, |t| t.min_buffer.max(1));
    let ceiling = tuning.as_ref().map_or(96, |t| t.max_buffer.max(floor));
    let short = TARGET_ARRIVAL_MARGIN - margin;
    if short > 0 {
        // Late, or about to be: grow at once, by the shortfall.
        config.client_tick_buffer =
            (config.client_tick_buffer + short as u64).clamp(floor, ceiling);
        *frames_over = 0;
    } else if margin > TARGET_ARRIVAL_MARGIN + 2 {
        // Comfortably early for a while: give back a tick.
        *frames_over += 1;
        if *frames_over >= SHRINK_EVERY_FRAMES && config.client_tick_buffer > floor {
            config.client_tick_buffer -= 1;
            *frames_over = 0;
        }
    } else {
        *frames_over = 0;
    }
}
