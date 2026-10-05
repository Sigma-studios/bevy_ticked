//! The session lifecycle: one state, derived from the role resources, and its doors.
//!
//! # What this replaces
//!
//! Each door used to be a system in `Update` with a run condition: `reset_on_host` on
//! `resource_added::<LocalServerPlayer>`, `reset_on_join` on `resource_added::<LocalClientPlayer>`,
//! `reset_on_leave` on either being removed. Three things were wrong with that shape, and every
//! one of them was found in a session rather than by reading it:
//!
//! - **Order against the tick.** `RunTickedLoop` runs before `Update`, so the frame a client role
//!   appeared ran a whole loop pass before its door did — and a snapshot forwarded in that frame's
//!   `PreUpdate` was applied onto the solo world, with nothing held and the old tick still running.
//!   Measured: forty joins out of forty. The door then ran and threw the host's world away.
//! - **Order against each other.** A leave and a join in one frame ran in no set order, and a leave
//!   that ran second undid the join. The host change worked around it by dropping both roles and
//!   waiting two whole frames before taking one back.
//! - **What they reset.** Each door listed the resources it put back. The lists drifted, and the
//!   comments record seven resources added to one of them after they had cost somebody an evening.
//!
//! # The shape now
//!
//! [`TickedSession`] is a Bevy state: `Offline`, `Solo`, `Host` or `Client`. It is *derived* — the
//! role resources [`LocalServerPlayer`], [`LocalClientPlayer`] and [`LocalSoloPlayer`] stay what
//! a game and a transport write and read, and a system at the head of `StateTransition` turns them
//! into the state. `StateTransition` runs after `PreUpdate` and before `RunFixedMainLoop`, so the
//! doors run strictly ordered — the old state's `OnExit`, then the new one's `OnEnter` — and
//! strictly before the first tick of the frame in which the role takes effect. No snapshot can
//! reach a world its door has not prepared.
//!
//! What each door resets is not listed here. Every door calls
//! [`reset_session_state`](bevy_ticked::session::reset_session_state), which resets whatever was
//! registered with [`SessionScope::Role`](bevy_ticked::session::SessionScope) and restarts the
//! clock; the leave also calls
//! [`end_session_state`](bevy_ticked::session::end_session_state) for what a game registered for
//! the whole session. The doors keep only what differs by role:
//!
//! - **The world.** A client never owns its world — it is the host's, a little in the past — so it
//!   is disposed of on the way into `Client` and on the way out. A leave disposes of it too. A host
//!   and a solo player own theirs: entering either keeps what stands, and raises the id allocator
//!   over it so that no id is issued twice.
//! - **Who the local player is.** [`LocalPlayer`] is the role's uuid, and `0` offline.
//! - **The clock.** A client is held until its host's world arrives; a host's clock is the
//!   session's.
//!
//! # A host change is a re-entry
//!
//! Setting the state to the one it is already in runs its `OnExit` and `OnEnter` again. A client
//! whose lobby changes host is re-entered as a client: the old host's world goes, and the new
//! host's arrives. A transport asks for that with [`restart_session`]; a new uuid under the same
//! role is treated the same way without asking.

use bevy::ecs::entity_disabling::Disabled;
use bevy::ecs::query::Allow;
use bevy::ecs::world::DeferredWorld;
use bevy::prelude::*;
use bevy::state::app::StatesPlugin;
use bevy::state::state::{StateTransitionEvent, StateTransitionSystems};

use bevy_ticked::{
    lifetimes::TrackedEntityLifetimes,
    resource_registry::TickedResourceRegistry,
    session::{SessionAppExt, SessionScope, end_session_state, forget_peer, reset_session_state},
    tick::{TickHoldReason, TickHolds},
    tracked_entity::{LocalSpawnerSlot, SpawnerSlot, TickTrackedEntity, TrackedIdAllocator},
};

use crate::client::{LocalClientPlayer, PendingSnapshot, PendingSnapshotTick};
use crate::input_plugin::LocalPlayer;
use crate::messages::PeerLeft;
use crate::server::LocalServerPlayer;
use crate::snapshot::SnapshotPacket;

/// This peer's part in a session. Derived from the role resources; see the module note.
///
/// Read it like any state — `in_state(TickedSession::Host)`, `OnEnter(TickedSession::Offline)` —
/// and change it by writing the role resources, not `NextState`: the derivation puts it back to
/// what the roles say on the next frame.
#[derive(States, Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum TickedSession {
    /// In no session: a menu, or a game that plays alone without saying so.
    #[default]
    Offline,
    /// Playing alone as a session: [`LocalSoloPlayer`] is present. The authority over its own
    /// world, sending nothing and never rolling back.
    Solo,
    /// Hosting: [`LocalServerPlayer`] is present.
    Host,
    /// A client: [`LocalClientPlayer`] is present.
    Client,
}

/// Resource identifying the local player of a solo session, the way [`LocalServerPlayer`] and
/// [`LocalClientPlayer`] identify a host's and a client's.
///
/// Its presence is what makes the session [`TickedSession::Solo`]. A transport's solo door
/// inserts it (the ensemble bridge's `StartSolo`); a game without one may insert it by hand. A
/// role resource wins over it: a solo player whose world is handed to a lobby is a host or a client
/// the frame the role is taken, and the bridge removes this in the same breath.
#[derive(Resource, Clone, Copy, Debug, PartialEq, Eq)]
pub struct LocalSoloPlayer(pub u128);

/// The doors, as a set, for a game that has a system of its own to run after one.
///
/// Every door is in it, in `OnEnter` and `OnExit` of every [`TickedSession`] state. A system in
/// `OnEnter(TickedSession::Offline).after(SessionDoor)` sees the world the leave left.
#[derive(SystemSet, Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SessionDoor;

/// The uuid the current state was entered under, so that a new one under the same role re-enters
/// it. The lifecycle's own bookkeeping, and the one thing here that outlives a session on purpose.
#[derive(Resource, Default, Debug)]
struct EnteredAs(Option<u128>);

/// That [`install`] has run.
#[derive(Resource, Default)]
struct SessionInstalled;

/// Install the lifecycle. Both role plugins call it; a listen server adds both.
pub(crate) fn install(app: &mut App) {
    if app.world().contains_resource::<SessionInstalled>() {
        return;
    }
    // Every consumer adds `DefaultPlugins`, or `StatesPlugin` with `MinimalPlugins`, before the
    // role plugins; a test that adds neither gets it here. After the role plugins it would be a
    // second copy, which Bevy refuses.
    if !app.is_plugin_added::<StatesPlugin>() {
        app.add_plugins(StatesPlugin);
    }
    app.init_resource::<SessionInstalled>()
        .init_resource::<EnteredAs>()
        .init_resource::<CarriedSnapshot>()
        .init_state::<TickedSession>()
        // Who the local player is belongs to the role; the entry door sets it after the reset.
        .init_session_resource_scoped::<LocalPlayer>(SessionScope::Role)
        .add_observer(forget_departed_peer)
        .add_systems(
            StateTransition,
            follow_the_roles.before(StateTransitionSystems::DependentTransitions),
        )
        .add_systems(OnExit(TickedSession::Solo), leave_role.in_set(SessionDoor))
        .add_systems(OnExit(TickedSession::Host), leave_role.in_set(SessionDoor))
        .add_systems(
            OnExit(TickedSession::Client),
            leave_client_role.in_set(SessionDoor),
        )
        .add_systems(OnEnter(TickedSession::Offline), leave.in_set(SessionDoor))
        .add_systems(OnEnter(TickedSession::Solo), enter_solo.in_set(SessionDoor))
        .add_systems(OnEnter(TickedSession::Host), enter_host.in_set(SessionDoor))
        .add_systems(
            OnEnter(TickedSession::Client),
            enter_client.in_set(SessionDoor),
        );
}

/// The state the role resources describe, and the uuid it is held under.
fn described(world: &World) -> (TickedSession, Option<u128>) {
    if let Some(server) = world.get_resource::<LocalServerPlayer>() {
        (TickedSession::Host, Some(server.0))
    } else if let Some(client) = world.get_resource::<LocalClientPlayer>() {
        (TickedSession::Client, Some(client.0))
    } else if let Some(solo) = world.get_resource::<LocalSoloPlayer>() {
        (TickedSession::Solo, Some(solo.0))
    } else {
        (TickedSession::Offline, None)
    }
}

/// The derivation: at the head of `StateTransition`, before anything is applied.
///
/// A pending request for the state the roles describe is left standing — that is a re-entry,
/// asked for by [`restart_session`]. Any other request is overruled: the roles are the truth.
fn follow_the_roles(world: &mut World) {
    let (wanted, uuid) = described(world);
    let current = *world.resource::<State<TickedSession>>().get();
    let entered_as = world.resource::<EnteredAs>().0;
    // Removed and inserted again since the last frame, even under the same uuid: a new session
    // under the same role, which runs its doors like any other. A resource written over in place
    // is not "added", so a game that re-inserts its role every frame does not re-enter every
    // frame.
    let readded = match wanted {
        TickedSession::Offline => false,
        TickedSession::Solo => world
            .get_resource_ref::<LocalSoloPlayer>()
            .is_some_and(|role| role.is_added()),
        TickedSession::Host => world
            .get_resource_ref::<LocalServerPlayer>()
            .is_some_and(|role| role.is_added()),
        TickedSession::Client => world
            .get_resource_ref::<LocalClientPlayer>()
            .is_some_and(|role| role.is_added()),
    };
    let mut next = world.resource_mut::<NextState<TickedSession>>();
    if matches!(*next, NextState::Pending(ref asked) if *asked == wanted) {
        return;
    }
    if wanted != current || (wanted != TickedSession::Offline && (entered_as != uuid || readded)) {
        next.set(wanted);
    } else {
        *next = NextState::Unchanged;
    }
}

/// Run the current state's `OnExit` and `OnEnter` again this frame, before the tick.
///
/// Removing a role resource and inserting it again — even with the same uuid — does the same
/// without asking: the derivation sees the resource added since it last ran. This is for a
/// transport that keeps the resource in place and still needs the doors.
///
/// For a transport whose session changed underneath the same role: a lobby's host changed, and a
/// client of the old host is a client of the new one with nothing of the old one kept. Called from
/// `PreUpdate` or earlier, it takes effect before this frame's first tick.
pub fn restart_session(world: &mut World) {
    let (wanted, _) = described(world);
    world.resource_mut::<NextState<TickedSession>>().set(wanted);
}

/// The transition being made, read in a door: `None` at startup, when the initial state is entered
/// from nothing.
fn this_transition(world: &World) -> Option<StateTransitionEvent<TickedSession>> {
    world
        .get_resource::<Messages<StateTransitionEvent<TickedSession>>>()?
        .iter_current_update_messages()
        .last()
        .cloned()
}

/// Every tracked entity goes, tombstones included, and the world's registered resources go back
/// to their defaults.
///
/// Tombstones too: a tombstone is `Disabled`, so a default query leaves it standing, and the
/// allocator goes back to zero below — the first ids of the next session would land on the last
/// one's graveyard. The allocator is a registered resource and is reset with the rest; it is named
/// here as well because the whole of "no id is issued twice" rests on it.
pub fn dispose_world(world: &mut World) {
    let stale: Vec<Entity> = world
        .query_filtered::<Entity, (With<TickTrackedEntity>, Allow<Disabled>)>()
        .iter(world)
        .collect();
    for entity in stale {
        world.despawn(entity);
    }
    if let Some(resources) = world.get_resource::<TickedResourceRegistry>().cloned() {
        resources.reset_all(world);
    }
    world.insert_resource(TrackedIdAllocator::default());
}

/// Keep the world that stands, as the session's own from its first tick: the allocator raised
/// over every id in it, and every one of them alive at tick 0.
///
/// A solo player opening their world to friends does have a claim on it. Zeroing the allocator
/// under a standing world hands the next mint an id that is already in use — and the snapshot keys
/// the whole world by id, so a rope that collides with a player has its components merged onto
/// that player and no rope is ever created. And with the histories cleared, nothing else says
/// these entities were ever born: a restore to tick 0 would tombstone every one of them.
fn keep_the_world(world: &mut World) {
    let held: Vec<TickTrackedEntity> = world
        .query::<&TickTrackedEntity>()
        .iter(world)
        .copied()
        .collect();
    let mut allocator = world.resource_mut::<TrackedIdAllocator>();
    for id in &held {
        allocator.raise_to(*id);
    }
    if let Some(mut lifetimes) = world.get_resource_mut::<TrackedEntityLifetimes>() {
        for id in &held {
            lifetimes.note_alive(bevy_ticked::tick_types::Tick::ZERO, id.0);
        }
    }
}

fn entered(world: &mut World, uuid: Option<u128>) {
    world.resource_mut::<EnteredAs>().0 = uuid;
    if let Some(uuid) = uuid {
        world.insert_resource(LocalPlayer(uuid));
    }
    bevy_ticked::capture_initial_state(world);
}

// ── out ──────────────────────────────────────────────────────────────────────

/// `OnExit(Solo)`, `OnExit(Host)`: nothing a role knew outlives it. The world is the next door's.
///
/// The spawner slot goes on the way out and not on the way in, unlike the registered state: a
/// host gives itself slot 0 as it enters, and a client is given its slot by its host's welcome,
/// which a transport may hold from before the role was taken.
///
/// A snapshot waiting to be applied is stashed first when the next state is a fresh `Client`: it
/// can only be the new host's, and `enter_client` puts it back. Without that, a practice game that
/// joins a lobby lost its first snapshot here, before the join door could keep it.
fn leave_role(world: &mut World) {
    let transition = this_transition(world);
    let joining = transition.as_ref().is_some_and(|transition| {
        transition.entered == Some(TickedSession::Client)
            && transition.exited != Some(TickedSession::Client)
    });
    if joining {
        let pending = take_pending(world);
        world.insert_resource(CarriedSnapshot(Some(pending)));
    }
    reset_session_state(world);
    world.remove_resource::<LocalSpawnerSlot>();
}

/// A snapshot carried from an exit door to the join door of the same transition. Never outlives
/// the transition: `enter_client` takes it, and nothing else writes it.
#[derive(Resource, Default)]
struct CarriedSnapshot(
    Option<(
        Option<SnapshotPacket>,
        Option<bevy_ticked::tick_types::Tick>,
    )>,
);

fn take_pending(
    world: &mut World,
) -> (
    Option<SnapshotPacket>,
    Option<bevy_ticked::tick_types::Tick>,
) {
    let packet = world
        .get_resource_mut::<PendingSnapshot>()
        .and_then(|mut pending| pending.0.take());
    let tick = world
        .get_resource_mut::<PendingSnapshotTick>()
        .and_then(|mut pending| pending.0.take());
    (packet, tick)
}

/// `OnExit(Client)`: and the world was the host's.
fn leave_client_role(world: &mut World) {
    leave_role(world);
    dispose_world(world);
}

/// `OnEnter(Offline)`: the session is over. Its world goes, and so does everything registered for
/// it, a game's included.
///
/// Not at startup, when `Offline` is entered from nothing and a game may already have built the
/// world it is about to play alone in.
///
/// The un-pause in `reset_session_state` is the part that bites when it is missing: a client
/// holds `AwaitingSync` until its first snapshot, and a peer that leaves before one arrives — a
/// refused join, a host that quits during the handshake — would otherwise sit paused for ever,
/// waiting on a session it is no longer in.
fn leave(world: &mut World) {
    if this_transition(world).is_none_or(|transition| transition.exited.is_none()) {
        return;
    }
    reset_session_state(world);
    world.remove_resource::<LocalSpawnerSlot>();
    dispose_world(world);
    end_session_state(world);
    world.resource_mut::<EnteredAs>().0 = None;
}

// ── in ───────────────────────────────────────────────────────────────────────

fn enter_solo(world: &mut World) {
    reset_session_state(world);
    keep_the_world(world);
    // Absent on a solo peer, which mints as the authority; see `LocalSpawnerSlot`.
    let uuid = world.get_resource::<LocalSoloPlayer>().map(|solo| solo.0);
    entered(world, uuid);
}

/// `OnEnter(Host)`: the session starts at tick 0, with whatever world stands. The invariant either
/// way — here and in [`enter_client`] — is that **no id is ever issued twice in a session.**
fn enter_host(world: &mut World) {
    reset_session_state(world);
    keep_the_world(world);
    world.insert_resource(LocalSpawnerSlot(SpawnerSlot::AUTHORITY));
    let uuid = world
        .get_resource::<LocalServerPlayer>()
        .map(|server| server.0);
    entered(world, uuid);
}

/// `OnEnter(Client)`: an empty world at tick 0, held until the host's arrives. The first snapshot
/// releases it. A game's own hold is a different reason and is neither set nor lifted here.
///
/// The world goes rather than being kept: whatever this peer built alone is about to be replaced
/// by the host's, and zeroing the allocator while it stands is how two entities come to share an
/// id and have their components merged by the first snapshot.
///
/// One thing is carried through the reset: a snapshot that arrived between the role being taken
/// and this door. Snapshots are only accepted by a peer holding the client role, so on a join from
/// `Offline` or `Solo` it can only be the new host's world, and dropping it costs the join a frame.
/// On a re-entry — a host change — it may be the old host's last word, and it goes.
fn enter_client(world: &mut World) {
    let rejoining = this_transition(world)
        .is_some_and(|transition| transition.exited == Some(TickedSession::Client));
    let carried = world
        .get_resource_mut::<CarriedSnapshot>()
        .and_then(|mut carried| carried.0.take());
    let arrived = if rejoining {
        None
    } else {
        // From `Solo` or `Host` the exit door stashed it; from `Offline` there was no exit door.
        Some(carried.unwrap_or_else(|| take_pending(world)))
    };
    reset_session_state(world);
    if let Some((packet, tick)) = arrived {
        if let Some(mut pending) = world.get_resource_mut::<PendingSnapshot>() {
            pending.0 = packet;
        }
        if let Some(mut pending) = world.get_resource_mut::<PendingSnapshotTick>() {
            pending.0 = tick;
        }
    }
    dispose_world(world);
    world
        .resource_mut::<TickHolds>()
        .hold(TickHoldReason::AwaitingSync);
    let uuid = world
        .get_resource::<LocalClientPlayer>()
        .map(|client| client.0);
    entered(world, uuid);
}

/// A client left a session that carries on: nothing held per sender may outlive it.
///
/// Its inputs at every tick (or the body it left behind keeps obeying its last keypress until the
/// window prunes it), its margin (or every snapshot keeps reporting a player who is not there),
/// its newest-tick mark (or a rejoin under the same uuid finds all of its inputs older than
/// "newest" and never gets one forward-filled), its baselines, its acks, its spawner slot. Whatever
/// is registered with [`PerPeer`](bevy_ticked::session::PerPeer), by this crate or a game.
fn forget_departed_peer(left: On<PeerLeft>, mut world: DeferredWorld) {
    forget_peer(&mut world, left.event().0);
}
