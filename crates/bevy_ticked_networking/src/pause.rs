//! Pausing a session, for everybody, on the authority's word.
//!
//! # What this replaces
//!
//! There was no session pause. A host that alt-tabbed on the web got one frame a second, or
//! none; every client kept ticking into a future the host had not produced, piled up two
//! seconds of lead, and then shed it at a couple of percent a second — twenty-five minutes of
//! the whole session feeling wrong, for a two-second tab switch. And a game that wanted a
//! pause menu had to build the replication of it itself; none did.
//!
//! # How it works
//!
//! [`SessionPause`] is a networked ticked resource: the authority's word about whether the
//! session is paused and at which tick. The host sets it from a [`PauseSession`] message (its
//! own, or a client's request the [`PausePolicy`] allows), holds its clock, and keeps
//! broadcasting; a client that receives it rolls back to the paused tick if it had predicted
//! past it, forgets that prediction, and holds. [`ResumeSession`] clears it; the host runs
//! again, and each client re-acquires its lead through the ordinary "at or behind" path,
//! which is a forward simulation of a few ticks and not a replay burst.
//!
//! The host pauses itself when its window loses focus (`window` feature) and when it notices
//! a gap in real time longer than [`PausePolicy::auto_pause_after_real_gap`]: it just came
//! back from a stall, and the pause tells every client to drop what it predicted in the
//! meantime. A client that has heard nothing for [`PausePolicy::client_soft_hold_after`]
//! holds on its own ([`TickHoldReason::SoftHold`]) rather than run ahead of a host that may
//! be gone; the next snapshot releases it.

use std::time::Duration;

use bevy::prelude::*;
use bevy_ticked::{
    TickedLoop, TickedSystems,
    tick::{CurrentTick, TickHoldReason, TickHolds},
};
use serde::{Deserialize, Serialize};

use crate::client::{ClientSet, LocalClientPlayer, SnapshotApplied};
use crate::server::LocalServerPlayer;

/// Why the session is paused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PauseReason {
    /// The host asked: a pause menu, a "ready?" screen.
    Host,
    /// The host's window lost focus.
    HostUnfocused,
    /// The host's frames stopped for a while and it is telling everyone to forget what they
    /// predicted in the meantime.
    HostStalled,
    /// A participant is away and the session waits for them (the lockstep phase).
    PeerAway(u128),
    /// A participant asked, and the policy allowed it.
    Participant(u128),
    /// A game's own reason.
    Custom(u8),
}

/// The pause, as the authority states it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Paused {
    /// The tick everybody holds at.
    pub at: u64,
    pub reason: PauseReason,
}

/// Whether the session is paused. Networked as `"bevy_ticked::SessionPause"`, registered by
/// both role plugins. Only the host writes it; a client reads it.
#[derive(Resource, Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionPause(pub Option<Paused>);

/// Ask for the session to pause. On the host it takes effect at the next loop pass; on a
/// client it is a request the host honours according to its [`PausePolicy`].
#[derive(Message, Clone, Copy, Debug, PartialEq, Eq)]
pub struct PauseSession(pub PauseReason);

/// Ask for the session to resume. Same rules as [`PauseSession`].
#[derive(Message, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ResumeSession;

/// Who may pause a session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WhoMayPause {
    /// Only the host. A client's request is ignored.
    HostOnly,
    /// Any participant; the host applies a client's request as [`PauseReason::Participant`].
    AnyParticipant,
}

/// The session's pause rules. A resource; the role plugins install the default.
#[derive(Resource, Clone, Copy, Debug, PartialEq, Eq)]
pub struct PausePolicy {
    pub who_may_pause: WhoMayPause,
    /// The host pauses when its window loses focus and resumes when it regains it. Needs the
    /// `window` feature to do anything.
    pub auto_pause_on_focus_loss: bool,
    /// The host pauses when a frame arrives this long after the previous one — it has been
    /// away — and resumes on the next frame, so clients discard what they predicted.
    pub auto_pause_after_real_gap: Option<Duration>,
    /// A client that has applied no snapshot for this long holds its clock until one comes.
    pub client_soft_hold_after: Option<Duration>,
}

impl Default for PausePolicy {
    fn default() -> Self {
        Self {
            who_may_pause: WhoMayPause::HostOnly,
            auto_pause_on_focus_loss: true,
            auto_pause_after_real_gap: Some(Duration::from_millis(500)),
            client_soft_hold_after: Some(Duration::from_millis(250)),
        }
    }
}

/// A client's pause request, for a transport to carry to the host. Triggered on the client
/// when a client writes [`PauseSession`] or [`ResumeSession`]; the transport sends it and
/// triggers [`ReceivedPauseRequest`] on the host.
#[derive(Event, Clone, Copy, Debug, PartialEq, Eq)]
pub struct SendPauseRequest {
    /// `Some(reason)` to pause, `None` to resume.
    pub pause: Option<PauseReason>,
}

/// A client's pause request as received by the host.
#[derive(Event, Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReceivedPauseRequest {
    pub sender: u128,
    pub pause: Option<PauseReason>,
}

pub(crate) fn install(app: &mut App) {
    if app.world().contains_resource::<PausePolicy>() {
        return;
    }
    app.init_resource::<PausePolicy>()
        .init_resource::<SessionPause>()
        .init_resource::<LastSnapshotHeard>()
        .init_resource::<AutoPaused>()
        .add_message::<PauseSession>()
        .add_message::<ResumeSession>()
        // Read by the soft hold; the client plugin adds it too, and a host-only app does not.
        .add_message::<SnapshotApplied>()
        .add_observer(honour_client_request)
        .add_systems(
            PreUpdate,
            (detect_real_gap, client_soft_hold, forward_client_requests),
        )
        .add_systems(
            TickedLoop,
            (
                (host_apply_requests, host_hold_at_pause)
                    .chain()
                    .in_set(TickedSystems::PreTick),
                client_follow_pause.in_set(ClientSet::AfterSnapshot),
            ),
        );
    #[cfg(feature = "window")]
    app.add_systems(PreUpdate, pause_on_focus_loss);
    {
        use crate::networked_registry::NetworkedTickedResourceAppExt;
        app.register_networked_ticked_resource::<SessionPause>("bevy_ticked::SessionPause");
    }
}

/// Which automatic pause the host currently holds, so it resumes only its own.
#[derive(Resource, Default, Debug, Clone, Copy, PartialEq, Eq)]
struct AutoPaused(Option<PauseReason>);

// ── the host ─────────────────────────────────────────────────────────────────

/// Pause and resume requests on the host, applied before the tick so the clock holds this
/// very pass.
fn host_apply_requests(world: &mut World) {
    if !world.contains_resource::<LocalServerPlayer>() {
        return;
    }
    let pauses: Vec<PauseReason> = world
        .resource_mut::<Messages<PauseSession>>()
        .drain()
        .map(|p| p.0)
        .collect();
    let resumes = world
        .resource_mut::<Messages<ResumeSession>>()
        .drain()
        .count();
    if pauses.is_empty() && resumes == 0 {
        return;
    }
    let tick = world.resource::<CurrentTick>().0;
    let mut pause = *world.resource::<SessionPause>();
    // The pause is at the *next* tick, which this pass still runs: every client has already
    // applied the current one, and a snapshot for a tick a client has seen is dropped as
    // stale at its door — a pause stamped there would never arrive. The tick after is news.
    if let Some(reason) = pauses.last().copied()
        && pause.0.is_none()
    {
        pause.0 = Some(Paused {
            at: tick + 1,
            reason,
        });
    }
    if resumes > 0 && pauses.is_empty() {
        pause.0 = None;
    }
    world.insert_resource(pause);
}

/// The host holds once it has reached the paused tick, and not before.
fn host_hold_at_pause(
    host: Option<Res<LocalServerPlayer>>,
    pause: Res<SessionPause>,
    tick: Res<CurrentTick>,
    mut holds: ResMut<TickHolds>,
) {
    if host.is_none() {
        return;
    }
    let held = pause.0.is_some_and(|paused| tick.0 >= paused.at);
    holds.set(TickHoldReason::SessionPause, held);
}

/// The host has been away: a frame arriving long after the last one. Pause at the tick the
/// host is still on, so clients forget what they predicted past it, and resume next frame.
fn detect_real_gap(
    time: Res<Time<Real>>,
    policy: Res<PausePolicy>,
    host: Option<Res<LocalServerPlayer>>,
    pause: Res<SessionPause>,
    mut auto: ResMut<AutoPaused>,
    mut pauses: MessageWriter<PauseSession>,
    mut resumes: MessageWriter<ResumeSession>,
) {
    if host.is_none() {
        return;
    }
    let Some(gap) = policy.auto_pause_after_real_gap else {
        return;
    };
    if time.delta() > gap {
        if pause.0.is_none() {
            pauses.write(PauseSession(PauseReason::HostStalled));
            auto.0 = Some(PauseReason::HostStalled);
        }
    } else if auto.0 == Some(PauseReason::HostStalled)
        && pause
            .0
            .is_some_and(|p| p.reason == PauseReason::HostStalled)
    {
        resumes.write(ResumeSession);
        auto.0 = None;
    }
}

/// The focus message only exists with a window plugin; a headless host has no window to lose.
#[cfg(feature = "window")]
fn pause_on_focus_loss(
    focus: Option<MessageReader<bevy::window::WindowFocused>>,
    policy: Res<PausePolicy>,
    host: Option<Res<LocalServerPlayer>>,
    pause: Res<SessionPause>,
    mut auto: ResMut<AutoPaused>,
    mut pauses: MessageWriter<PauseSession>,
    mut resumes: MessageWriter<ResumeSession>,
) {
    let Some(mut focus) = focus else {
        return;
    };
    if host.is_none() || !policy.auto_pause_on_focus_loss {
        focus.clear();
        return;
    }
    for event in focus.read() {
        if !event.focused && pause.0.is_none() {
            pauses.write(PauseSession(PauseReason::HostUnfocused));
            auto.0 = Some(PauseReason::HostUnfocused);
        } else if event.focused
            && auto.0 == Some(PauseReason::HostUnfocused)
            && pause
                .0
                .is_some_and(|p| p.reason == PauseReason::HostUnfocused)
        {
            resumes.write(ResumeSession);
            auto.0 = None;
        }
    }
}

/// A client's request, as the transport delivered it, honoured per policy.
fn honour_client_request(
    request: On<ReceivedPauseRequest>,
    policy: Res<PausePolicy>,
    host: Option<Res<LocalServerPlayer>>,
    mut pauses: MessageWriter<PauseSession>,
    mut resumes: MessageWriter<ResumeSession>,
) {
    if host.is_none() || policy.who_may_pause != WhoMayPause::AnyParticipant {
        return;
    }
    let request = *request.event();
    match request.pause {
        Some(_) => {
            pauses.write(PauseSession(PauseReason::Participant(request.sender)));
        }
        None => {
            resumes.write(ResumeSession);
        }
    }
}

// ── the client ───────────────────────────────────────────────────────────────

/// A client's own `PauseSession`/`ResumeSession` become requests for the transport to carry.
fn forward_client_requests(
    client: Option<Res<LocalClientPlayer>>,
    mut pauses: MessageReader<PauseSession>,
    mut resumes: MessageReader<ResumeSession>,
    mut commands: Commands,
) {
    if client.is_none() {
        return;
    }
    for pause in pauses.read() {
        commands.trigger(SendPauseRequest {
            pause: Some(pause.0),
        });
    }
    for _ in resumes.read() {
        commands.trigger(SendPauseRequest { pause: None });
    }
}

/// After the snapshot: hold at the paused tick, rolling back to it if this client had
/// predicted past it; release when the authority resumes.
fn client_follow_pause(world: &mut World) {
    if !world.contains_resource::<LocalClientPlayer>() {
        return;
    }
    let pause = *world.resource::<SessionPause>();
    match pause.0 {
        Some(paused) => {
            let current = world.resource::<CurrentTick>().0;
            if current < paused.at {
                // Not there yet: run up to it, then hold.
                world
                    .resource_mut::<TickHolds>()
                    .release(TickHoldReason::SessionPause);
                return;
            }
            if current > paused.at {
                // Forget the prediction: what the host did not produce never happened.
                bevy_ticked::rollback::rollback_to_tick(world, paused.at);
                let registry = world
                    .resource::<bevy_ticked::registry::TickedComponentRegistry>()
                    .clone();
                registry.truncate_all_after(world, paused.at);
                bevy_ticked::events::TickedEventRegistry::truncate_all_after(world, paused.at);
            }
            world
                .resource_mut::<TickHolds>()
                .hold(TickHoldReason::SessionPause);
        }
        None => {
            world
                .resource_mut::<TickHolds>()
                .release(TickHoldReason::SessionPause);
        }
    }
}

/// When the last snapshot was applied, on the frame clock.
#[derive(Resource, Default, Debug, Clone, Copy)]
struct LastSnapshotHeard(Option<Duration>);

/// A client that has heard nothing for a while holds rather than run ahead of a host that
/// may be gone; the next snapshot releases it.
fn client_soft_hold(
    time: Res<Time<Real>>,
    policy: Res<PausePolicy>,
    client: Option<Res<LocalClientPlayer>>,
    mut applied: MessageReader<SnapshotApplied>,
    mut heard: ResMut<LastSnapshotHeard>,
    mut holds: ResMut<TickHolds>,
) {
    if client.is_none() {
        heard.0 = None;
        applied.clear();
        return;
    }
    let now = time.elapsed();
    if applied.read().next().is_some() {
        heard.0 = Some(now);
        holds.release(TickHoldReason::SoftHold);
        return;
    }
    let Some(limit) = policy.client_soft_hold_after else {
        return;
    };
    match heard.0 {
        Some(last) if now.saturating_sub(last) > limit => {
            holds.hold(TickHoldReason::SoftHold);
        }
        _ => {}
    }
}
