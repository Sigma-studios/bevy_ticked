//! Becoming a host, becoming a client, playing alone, and stopping being any of them.
//!
//! # Why this belongs here and not in the game
//!
//! `bevy_ticked_networking` has three role resources and a [`TickedSession`] state derived from
//! them; `bevy_ensemble` has lobbies, participants and a local uuid. Mapping the second onto the
//! first is the one thing that turns two independent crates into a session, and until it lived here
//! it was left to the consumer — so **every** consumer wrote it, including both examples in this
//! repository, and they wrote it differently in the places that are hardest to get right.
//!
//! The differences are not cosmetic:
//!
//! - **When to adopt.** Keying off [`Lobby`] is the obvious choice and it is late: a lobby is
//!   promoted only after a handshake cooldown, and the data channel is up well before that. There
//!   is a window in which the host's world arrives at a peer that still believes it is playing
//!   alone, and everything downstream of the local uuid — which body is mine, which gets the
//!   camera, which is drawn as somebody else — is wrong for its duration.
//!   [`adopt_role`] keys off [`LocalMultiplayerPlayerId`] instead, which appears the moment the
//!   signalling server says the lobby was joined, and is strictly earlier than any peer connection
//!   can exist.
//!
//! - **The window in between.** Between a lobby appearing and a role being adopted, a peer is
//!   still nominally the authority over its own world *and* the participant roster has already
//!   arrived. Anything that mints a tracked entity there is doing a client-side spawn: the next
//!   snapshot despawns it, the system spawns it again, and the two chase each other at the
//!   snapshot rate. [`may_spawn_tracked`] is the guard, and it is strictly stricter than "am I the
//!   authority".
//!
//! # Playing alone is a session
//!
//! Both games that can play alone wrote the same four things by hand: a `SoloPlayer` resource and
//! a `LocalPlayer` set to a made-up uuid on the way in, a `reset_on_leave` system with a
//! `Local<bool>` that watched for "no lobby and no solo" to put the game back on the way out, and
//! a rule that a lobby appearing under a solo game ends it. None of it was wrong, and all of it was
//! a second session lifecycle beside this one, with its own doors and none of the ordering.
//!
//! So solo is a state of [`TickedSession`] like the others: [`StartSolo`] enters it under
//! [`TickedEnsembleSessionPlugin::solo_uuid`], [`EndSession`] leaves it — through the same leave as
//! any session, so whatever a game registered with `init_session_resource` goes back — and a lobby
//! appearing hands the solo world over rather than ending it: the peer stays the authority over it
//! until the lobby gives it a role, and then a host keeps the world it built and a client has it
//! replaced by the host's. A lobby that goes before giving a role — a refused host request, a join
//! that never completed — hands the world back: the player is still playing alone.
//!
//! # When the host changes
//!
//! A lobby that migrates keeps its entity when its host goes, and another member hosts it. The
//! snapshot model cannot carry a match across that: the authoritative world lived on the old host,
//! and a client only ever held what it was sent, a little in the past. So a [`HostChanged`] is a
//! door: every survivor leaves its role and enters its new one in the same frame — the new host as
//! a host of an empty world, every other member as a client of it again — with the lobby standing
//! throughout. A game shows it as its lobby screen.
//!
//! It used to be done by dropping both roles, waiting two whole frames so that the leave keyed on
//! the removal had certainly run before the join keyed on the addition, and taking a role back
//! once the new host was reached. The wait is gone with the race: the doors run in order now, in
//! `StateTransition`, before the tick. The "reached" half is still needed and lives in the
//! handshake, which does not start its clock on a client until the lobby has stopped
//! [`AwaitingHost`].
//!
//! # Opt in, rather than automatic
//!
//! [`TickedEnsembleSessionPlugin`] is separate from
//! [`TickedNetworkingEnsemblePlugin`](crate::TickedNetworkingEnsemblePlugin) on purpose. A
//! consumer that already adopts roles by hand would otherwise find this crate doing it too — and
//! the despawn at a join is not something to start doing to somebody's world without being asked.

use core::time::Duration;

use bevy::prelude::*;
use bevy_ensemble::prelude::*;
// `PeerRtt` / `PeerRttJitter` come in via the prelude above; named here so the reason they are
// wanted is legible at the import site.
use bevy_ensemble::{AwaitingHost, HostChanged, LobbyClientPlayerUuid, PeerRtt, PeerRttJitter};
use bevy_ticked::prelude::*;
use bevy_ticked::time::{Ticked, TickedTime};
use bevy_ticked_networking::client::{AppliedSnapshotTick, ClientTickBuffer, LocalClientPlayer};
use bevy_ticked_networking::messages::PeerLeft;
use bevy_ticked_networking::server::{LocalServerPlayer, SnapshotRecipientList};
use bevy_ticked_networking::session::{
    LocalSoloPlayer, SessionDoor, TickedSession, restart_session,
};

use crate::handshake::{
    HandshakeTimedOut, HandshakeTimeout, PendingWelcome, RegistryMismatch, RegistryVerified,
    TickedPeerVerified,
};

/// Adopt and release the ticked role from the ensemble lobby, run the registry handshake, keep
/// [`SnapshotRecipientList`] current, and play alone on request.
///
/// Add alongside [`TickedNetworkingEnsemblePlugin`](crate::TickedNetworkingEnsemblePlugin) to stop
/// writing session bookkeeping by hand.
pub struct TickedEnsembleSessionPlugin {
    /// How long a client waits for its host's registries before giving up. See
    /// [`HandshakeTimeout`].
    pub handshake_timeout: Duration,
    /// The uuid a solo session plays under: what [`LocalPlayer`] and
    /// [`LocalSoloPlayer`] hold after [`StartSolo`]. `1` by default, which is what both games that
    /// play alone chose, and which no signalling server hands out.
    ///
    /// [`LocalPlayer`]: bevy_ticked_networking::input_plugin::LocalPlayer
    pub solo_uuid: u128,
}

impl Default for TickedEnsembleSessionPlugin {
    fn default() -> Self {
        Self {
            handshake_timeout: Duration::from_secs(5),
            solo_uuid: 1,
        }
    }
}

/// Runtime copy of [`TickedEnsembleSessionPlugin::solo_uuid`].
#[derive(Resource, Clone, Copy, Debug, PartialEq, Eq)]
pub struct SoloUuid(pub u128);

/// Start playing alone: this peer becomes [`TickedSession::Solo`], the authority over its own
/// world, under [`SoloUuid`]. Ignored while in any other session or with a lobby forming — leave
/// that first.
#[derive(Message, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StartSolo;

/// Leave whatever session this peer is in: a solo one, a lobby it hosts or joined, or one still
/// forming. The lobby entities are despawned and the roles dropped; the leave door does the rest.
#[derive(Message, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EndSession;

impl Plugin for TickedEnsembleSessionPlugin {
    fn build(&self, app: &mut App) {
        install_lobby_tracking(app);
        app.init_resource::<SnapshotRecipientList>()
            .insert_resource(HandshakeTimeout(self.handshake_timeout))
            .insert_resource(SoloUuid(self.solo_uuid))
            .add_message::<StartSolo>()
            .add_message::<EndSession>()
            .add_plugins(crate::handshake::plugin)
            .init_session_resource_scoped::<TickBufferSeeded>(SessionScope::Role)
            .remove_at_session_end::<SoloHandedToLobby>(SessionScope::Role)
            .init_session_resource::<Visiting>()
            .add_observer(forget_departed_client)
            // Sized at the join door itself, before the first tick: a snapshot that arrived with
            // the role is applied in that tick and closes the `Update` window below.
            .add_systems(
                OnEnter(TickedSession::Client),
                seed_tick_buffer.after(SessionDoor),
            )
            .add_systems(
                PreUpdate,
                follow_host_change.after(bevy_ensemble::EnsembleSet::ReceivePackets),
            )
            .add_systems(
                Update,
                (
                    end_session,
                    start_solo,
                    hand_solo_to_a_lobby,
                    adopt_role,
                    seed_tick_buffer,
                    release_role,
                    forget_mismatch,
                    end_a_visit_without_a_role,
                    list_recipients,
                )
                    .chain(),
            );
    }
}

/// The entity carrying `Lobby` while this peer is in a session, host or client.
///
/// One resource rather than a `Single<Entity, With<Lobby>>` in every system that sends: the
/// bridge, the handshake and a game all want the same entity, and a `Single` that finds none
/// on the frame a lobby is replaced skips the system silently. Inserted with the `Lobby`
/// component and removed with it.
#[derive(Resource, Clone, Copy, Debug, PartialEq, Eq)]
pub struct TickedSessionLobby(pub Entity);

/// Keep [`TickedSessionLobby`] true. Added by both plugins of this crate, once.
pub(crate) fn install_lobby_tracking(app: &mut App) {
    if app.is_plugin_added::<LobbyTrackingPlugin>() {
        return;
    }
    app.add_plugins(LobbyTrackingPlugin);
}

struct LobbyTrackingPlugin;

impl Plugin for LobbyTrackingPlugin {
    fn build(&self, app: &mut App) {
        app.add_observer(|add: On<Add, Lobby>, mut commands: Commands| {
            commands.insert_resource(TickedSessionLobby(add.entity));
        })
        .add_observer(
            |remove: On<Remove, Lobby>,
             lobby: Option<Res<TickedSessionLobby>>,
             mut commands: Commands| {
                if lobby.is_some_and(|lobby| lobby.0 == remove.entity) {
                    commands.remove_resource::<TickedSessionLobby>();
                }
            },
        );
    }
}

/// True when this peer holds neither the host nor the client role — playing alone, or not in a
/// session yet.
pub fn is_solo(
    server: Option<Res<LocalServerPlayer>>,
    client: Option<Res<LocalClientPlayer>>,
) -> bool {
    server.is_none() && client.is_none()
}

/// True when this peer decides what is true: a host, or a solo player.
///
/// The opposite of "is a client", rather than "is a server", because a peer with no lobby at all
/// is the authority over its own world.
pub fn is_authoritative(client: Option<Res<LocalClientPlayer>>) -> bool {
    client.is_none()
}

/// True while this peer is in a session of any kind, including one still forming: playing alone,
/// hosting, a client, or with a lobby it asked to host or join and has not yet been given a role
/// in.
///
/// What a game's screens follow. It is `false` exactly when the leave has run — or, for a lobby
/// that never gave a role, is about to run this frame — so a menu shown on `!in_session` is shown
/// over a world that has been put back.
pub fn in_session(
    state: Res<State<TickedSession>>,
    lobbies: Query<(), Or<(With<Lobby>, With<PendingLobby>)>>,
) -> bool {
    *state.get() != TickedSession::Offline || !lobbies.is_empty()
}

/// True when this peer may mint a tracked entity **right now**.
///
/// Stricter than [`is_authoritative`], and the difference is the join window described in the
/// module note: a solo peer with a lobby forming around it is about to stop being the authority,
/// and anything it spawns in the meantime is a client-side spawn that rollback cannot undo.
///
/// So: a host always may, a client never may, and a peer with no role may only while there is no
/// lobby of any kind.
pub fn may_spawn_tracked(
    server: Option<Res<LocalServerPlayer>>,
    client: Option<Res<LocalClientPlayer>>,
    lobbies: Query<(), Or<(With<Lobby>, With<PendingLobby>)>>,
) -> bool {
    if client.is_some() {
        return false;
    }
    if server.is_some() {
        return true;
    }
    lobbies.is_empty()
}

/// End this peer's ticked session, whatever it is: every role dropped, solo included, and every
/// host-side mark of a verified client taken off.
///
/// Dropping the roles is what does the work: the state follows them to
/// [`TickedSession::Offline`], and the leave door clears the world, the clock and everything
/// registered for the session. The lobby, if there is one, is left to the caller.
pub fn end_ticked_session(world: &mut World) {
    world.remove_resource::<LocalServerPlayer>();
    world.remove_resource::<LocalClientPlayer>();
    world.remove_resource::<LocalSoloPlayer>();
    unverify_clients(world);
}

fn unverify_clients(world: &mut World) {
    let verified: Vec<Entity> = world
        .query_filtered::<Entity, With<TickedPeerVerified>>()
        .iter(world)
        .collect();
    for entity in verified {
        world.entity_mut(entity).remove::<TickedPeerVerified>();
    }
}

/// Forget what this peer knew about its host's registries: a new host has to be verified afresh.
fn forget_the_host(world: &mut World) {
    world.remove_resource::<RegistryMismatch>();
    world.remove_resource::<HandshakeTimedOut>();
    world.remove_resource::<RegistryVerified>();
    world.resource_mut::<PendingWelcome>().0 = None;
}

/// A solo world whose lobby is forming: this peer is still its authority, and the world goes to
/// whichever role the lobby gives. If the lobby goes first, the solo session carries on.
#[derive(Resource, Clone, Copy, Debug, Default)]
pub struct SoloHandedToLobby;

/// Whether this peer has been in a session — a role, solo, or a lobby — since it last left one.
/// What lets a lobby that never gave a role still end in a leave.
#[derive(Resource, Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Visiting(bool);

impl SessionReset for Visiting {}

// ── the doors a game asks for ───────────────────────────────────────────────

fn start_solo(
    mut commands: Commands,
    mut starts: MessageReader<StartSolo>,
    uuid: Res<SoloUuid>,
    state: Res<State<TickedSession>>,
    server: Option<Res<LocalServerPlayer>>,
    client: Option<Res<LocalClientPlayer>>,
    lobbies: Query<(), Or<(With<Lobby>, With<PendingLobby>)>>,
) {
    if starts.read().last().is_none() {
        return;
    }
    if *state.get() != TickedSession::Offline
        || server.is_some()
        || client.is_some()
        || !lobbies.is_empty()
    {
        warn!(
            "StartSolo while already in a session ({:?}); ignored",
            state.get()
        );
        return;
    }
    commands.insert_resource(LocalSoloPlayer(uuid.0));
}

/// Leave: the lobbies despawned, as both games' `leave` did, and the roles dropped in the same
/// flush so the leave door runs this frame rather than after the lobby's removal is noticed.
fn end_session(
    mut commands: Commands,
    mut ends: MessageReader<EndSession>,
    lobbies: Query<Entity, Or<(With<Lobby>, With<PendingLobby>)>>,
) {
    if ends.read().last().is_none() {
        return;
    }
    for lobby in &lobbies {
        commands.entity(lobby).try_despawn();
    }
    commands.queue(end_ticked_session);
}

/// A lobby appearing under a solo game hands the world to it. See the module note.
fn hand_solo_to_a_lobby(
    mut commands: Commands,
    solo: Option<Res<LocalSoloPlayer>>,
    handed: Option<Res<SoloHandedToLobby>>,
    lobbies: Query<(), Or<(With<Lobby>, With<PendingLobby>)>>,
) {
    if solo.is_some() && handed.is_none() && !lobbies.is_empty() {
        commands.insert_resource(SoloHandedToLobby);
    }
}

// ── the lobby's doors ────────────────────────────────────────────────────────

/// The lobby changed host: every role is left and taken again, in this frame, before the tick.
///
/// The new host hosts an empty world — a client's copy of the old host's was never its own — and
/// every other member is a client of it, waiting for its world. What the handshake knew was about
/// the old host and goes with it: a mismatch or a timeout gets a fresh chance with the new one.
/// A peer that held no role is left to [`adopt_role`], which takes one as usual.
fn follow_host_change(mut commands: Commands, mut changes: MessageReader<HostChanged>) {
    let Some(change) = changes.read().last().cloned() else {
        return;
    };
    info!(
        "the lobby's host changed from {:#x} to {:#x}; the ticked session starts over with it",
        change.previous, change.new
    );
    commands.queue(move |world: &mut World| {
        forget_the_host(world);
        let uuid = world
            .get_resource::<LocalServerPlayer>()
            .map(|role| role.0)
            .or_else(|| world.get_resource::<LocalClientPlayer>().map(|role| role.0));
        let Some(uuid) = uuid else {
            return;
        };
        unverify_clients(world);
        world.remove_resource::<LocalServerPlayer>();
        world.remove_resource::<LocalClientPlayer>();
        if change.promoted {
            world.insert_resource(LocalServerPlayer(uuid));
        } else {
            world.insert_resource(LocalClientPlayer(uuid));
        }
        restart_session(world);
    });
}

/// Take the host or client role as soon as this peer knows which body is its own.
///
/// The world is the door's business: a client's `OnEnter` disposes of whatever this peer built
/// alone, a host's keeps it. A solo player's marker goes in the same flush as the role is taken,
/// so the state goes straight from `Solo` to the role and nothing in between runs a leave.
fn adopt_role(
    mut commands: Commands,
    local_player: Option<Res<LocalMultiplayerPlayerId>>,
    server: Option<Res<LocalServerPlayer>>,
    client: Option<Res<LocalClientPlayer>>,
    mismatch: Option<Res<RegistryMismatch>>,
    timed_out: Option<Res<HandshakeTimedOut>>,
    hosting: Query<(), (With<Host>, Or<(With<Lobby>, With<PendingLobby>)>)>,
    // A client adopts on the promoted lobby, not the pending one: until the backend's own
    // handshake has run, the data channel may not carry anything, and a role taken then
    // starts the registry handshake's clock on a link that cannot deliver it yet.
    joined: Query<(), (Without<Host>, With<Lobby>)>,
) {
    if server.is_some() || client.is_some() {
        return;
    }
    // A session this peer cannot speak the language of, or never heard from, does not get
    // retried at frame rate.
    if mismatch.is_some() || timed_out.is_some() {
        return;
    }
    let Some(local_player) = local_player else {
        return;
    };
    // bevy_ensemble used to give a host a placeholder identity the frame it asked to host, and
    // a role adopted from it kept a uuid of zero for the whole session. There is no placeholder
    // any more: the resource is absent until the backend knows the identity, which is what the
    // `let Some` above waits for.
    let is_host = !hosting.is_empty();
    if !is_host && joined.is_empty() {
        return;
    }
    commands.remove_resource::<LocalSoloPlayer>();
    if is_host {
        commands.insert_resource(LocalServerPlayer(local_player.0));
    } else {
        commands.insert_resource(LocalClientPlayer(local_player.0));
    }
}

/// That this client's prediction buffer has been sized from its link this session. Registered
/// with the session, where it used to be a `Local<bool>`: a `Local` that only re-armed on a frame
/// with no client role never re-armed across a host change, which keeps the role.
#[derive(Resource, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TickBufferSeeded(pub bool);

impl SessionReset for TickBufferSeeded {}

/// Size the client's prediction buffer from the connection, before the first input has made
/// the trip that would let it measure itself.
///
/// [`ClientTickBuffer`] adapts from the server's report of how early each client's input
/// arrives, which is the right signal and the only one that needs no transport — but it is a
/// signal that does not exist until a client has been playing for a moment. Until then the
/// buffer is a constant, and a constant is a guess about a link nobody has looked at: the
/// default six ticks is a 62 ms round trip, and a client whose one-way trip is longer than
/// that starts the session already behind the authority.
///
/// `bevy_ensemble` has both numbers because it pings, so the bridge between the two crates is
/// where they meet. `bevy_ticked_networking` stays transport-free, which is the property that
/// makes it usable over anything.
///
/// Jitter matters more than the mean here, and is the reason this touches the margin at all.
/// The margin is headroom for the *unlucky* packet, and it was a hardcoded two ticks — under
/// water on any link with more than ~30 ms of variation, where every spike costs a keypress.
///
/// # The window
///
/// Only until the client has applied its first snapshot: after that the buffer holds
/// measurements, and a seed is a guess that would be overwriting them. First at the join door
/// itself (`OnEnter(Client)`), where a lobby that has pinged already has the numbers; then every
/// frame until one comes back. Keyed on the first snapshot rather than the `AwaitingSync` hold,
/// which a snapshot that arrived with the role releases before `Update` ever sees it held. Pings start on the first frame a lobby exists, so a
/// sample is usually there in time — and when it isn't, nothing happens and the default is used.
/// That is survivable rather than free: it costs one correction shortly after the join.
fn seed_tick_buffer(
    client: Option<Res<LocalClientPlayer>>,
    applied: Res<AppliedSnapshotTick>,
    ticked: Res<Time<Ticked>>,
    connection: Query<(&PeerRtt, Option<&PeerRttJitter>), With<Lobby>>,
    mut buffer: ResMut<ClientTickBuffer>,
    mut seeded: ResMut<TickBufferSeeded>,
) {
    if client.is_none() || seeded.0 || applied.0.is_some() {
        return;
    }
    let Some((rtt, jitter)) = connection.iter().next() else {
        // No ping has come back yet. Try again next frame, until the first snapshot closes the
        // window.
        return;
    };
    seeded.0 = true;
    let jitter = jitter.map_or(Duration::ZERO, |jitter| jitter.0);
    buffer.seed_from_rtt(rtt.0, jitter, ticked.timestep());
    debug!(
        "sized the prediction buffer from the link: rtt {:.0?}, jitter {:.0?} -> \
         replay distance {} ticks, margin {} ticks",
        rtt.0, jitter, buffer.target_replay_distance, buffer.target_margin,
    );
}

/// Give the role back when the lobby goes, however it went — and give a solo session back its
/// world when the lobby it was handed to goes before giving a role.
///
/// That last case is a host request the server refused, or a join that never completed, from a
/// practice game. The world was still the solo player's — no role had taken it — so the player is
/// put back where they were, still playing alone, rather than thrown out of practice by a network
/// error.
///
/// A state check rather than `RemovedComponents<Lobby>`, because a refused join despawns an entity
/// that never carried [`Lobby`] at all — the removal never fires, and the peer sits in a session
/// that does not exist.
fn release_role(
    mut commands: Commands,
    server: Option<Res<LocalServerPlayer>>,
    client: Option<Res<LocalClientPlayer>>,
    handed: Option<Res<SoloHandedToLobby>>,
    lobbies: Query<(), Or<(With<Lobby>, With<PendingLobby>)>>,
) {
    if !lobbies.is_empty() {
        return;
    }
    if server.is_some() || client.is_some() {
        commands.queue(end_ticked_session);
    } else if handed.is_some() {
        commands.remove_resource::<SoloHandedToLobby>();
    }
}

/// Forget a registry mismatch or a handshake timeout once the lobby it belonged to is gone.
///
/// Joining a *different* lobby is allowed to try again — the peer on the other end of that one may
/// well have been built from the same commit as this.
fn forget_mismatch(
    mut commands: Commands,
    mismatch: Option<Res<RegistryMismatch>>,
    timed_out: Option<Res<HandshakeTimedOut>>,
    lobbies: Query<(), Or<(With<Lobby>, With<PendingLobby>)>>,
) {
    if !lobbies.is_empty() {
        return;
    }
    if mismatch.is_some() {
        commands.remove_resource::<RegistryMismatch>();
    }
    if timed_out.is_some() {
        commands.remove_resource::<HandshakeTimedOut>();
    }
}

/// A lobby that goes without ever giving this peer a role still ends a session: a refused join, a
/// host request the server turned down. There is no role to drop, so the leave is asked for by
/// entering `Offline` again, which runs its door like any other leave.
///
/// What the games' own `reset_on_leave` did by watching "no lobby and no solo", and the one case
/// the role-derived state cannot see on its own.
fn end_a_visit_without_a_role(
    mut visiting: ResMut<Visiting>,
    state: Res<State<TickedSession>>,
    server: Option<Res<LocalServerPlayer>>,
    client: Option<Res<LocalClientPlayer>>,
    solo: Option<Res<LocalSoloPlayer>>,
    lobbies: Query<(), Or<(With<Lobby>, With<PendingLobby>)>>,
    mut next: ResMut<NextState<TickedSession>>,
) {
    let in_one = *state.get() != TickedSession::Offline
        || server.is_some()
        || client.is_some()
        || solo.is_some()
        || !lobbies.is_empty();
    if in_one {
        if !visiting.0 {
            visiting.0 = true;
        }
        return;
    }
    if visiting.0 {
        // Reset by the leave itself: it is a session resource.
        next.set(TickedSession::Offline);
    }
}

/// Tell the server a client is gone, the moment the lobby crate knows it.
///
/// A [`LobbyClient`] lives only on the host, and every way a client can go — kicked, timed out
/// by liveness, or leaving of its own accord — ends with the backend despawning it. That is the
/// one place all the ways meet, so it is the one place to write [`PeerLeft`], which is what
/// makes the server forget the departed uuid's inputs, margin and spawner slot. Read here rather
/// than on the participant entity because the participant is despawned by a command queued *from*
/// this same removal, one flush later, and the uuid is still on the client entity while `Remove`
/// runs.
fn forget_departed_client(
    remove: On<Remove, LobbyClient>,
    uuids: Query<&LobbyClientPlayerUuid>,
    mut commands: Commands,
) {
    if let Ok(uuid) = uuids.get(remove.entity) {
        commands.trigger(PeerLeft(uuid.0));
    }
}

/// Who gets a snapshot: every `LobbyClient` whose registries matched, by uuid, ascending.
///
/// Rebuilt from the entities each frame rather than edited on add and remove, so a client that
/// was despawned by the transport without ever being unverified is gone from the list the same
/// frame. Written only when it changed, so `Res::is_changed` on it means something.
fn list_recipients(
    mut recipients: ResMut<SnapshotRecipientList>,
    verified: Query<&LobbyClientPlayerUuid, (With<LobbyClient>, With<TickedPeerVerified>)>,
) {
    let mut uuids: Vec<u128> = verified.iter().map(|client| client.0).collect();
    uuids.sort_unstable();
    if recipients.0 != uuids {
        recipients.0 = uuids;
    }
}

/// Whether this client's lobby is still waiting to reach a new host. The handshake does not count
/// the wait against its timeout; see [`HandshakeTimeout`].
pub(crate) fn awaiting_new_host(world: &mut World) -> bool {
    world
        .query_filtered::<(), (With<Lobby>, With<AwaitingHost>)>()
        .iter(world)
        .next()
        .is_some()
}

#[cfg(test)]
mod tests {
    //! Adoption against the real lobby crate, with the backend's part played by hand.
    use super::*;
    use bevy_ensemble::{RequestLobby, StartHosting};
    use bevy_ticked_networking::client::TickedClientPlugin;
    use bevy_ticked_networking::server::TickedServerPlugin;
    use serde::{Deserialize, Serialize};

    #[derive(Clone, Serialize, Deserialize)]
    struct Input;

    /// The stack a consumer builds: the lobby crate, the tick loop, both roles, the bridge and
    /// this plugin. No transport: what a backend would do to the world is done by hand below.
    fn app() -> App {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins)
            .add_plugins(EnsemblePlugin)
            .add_plugins(TickedPlugin {
                source: TickSource::Hz(64.0),
                ..default()
            })
            .add_plugins(TickedServerPlugin::<Input>::new())
            .add_plugins(TickedClientPlugin::<Input>::new())
            .add_plugins(crate::TickedNetworkingEnsemblePlugin::<Input>::new())
            .add_plugins(TickedEnsembleSessionPlugin::default());
        app
    }

    /// What `bevy_ensemble_webrtc` does on `LobbyCreated`: the signalling server's uuid for
    /// the local player, and the pending host lobby promoted.
    fn lobby_created(app: &mut App, uuid: u128) {
        app.world_mut()
            .insert_resource(LocalMultiplayerPlayerId(uuid));
        let mut pending = app
            .world_mut()
            .query_filtered::<Entity, (With<PendingLobby>, With<Host>)>();
        let lobby = pending.single(app.world()).expect("a pending host lobby");
        app.world_mut()
            .entity_mut(lobby)
            .remove::<(PendingLobby, RequestLobby)>()
            .insert(Lobby);
    }

    fn host_role(world: &World) -> Option<u128> {
        world.get_resource::<LocalServerPlayer>().map(|role| role.0)
    }

    /// The uuid the roster gives the host is the one its bodies are owned by, and the one its
    /// inputs are queued under has to be the same. Between `StartHosting` and the lobby's
    /// creation the lobby crate holds a placeholder, and a role taken from it kept it for the
    /// whole session.
    #[test]
    fn a_host_is_adopted_from_the_backend_identity_and_not_the_placeholder() {
        let mut app = app();
        app.world_mut().write_message(StartHosting);
        for _ in 0..3 {
            app.update();
        }
        assert!(
            app.world()
                .get_resource::<LocalMultiplayerPlayerId>()
                .is_none(),
            "until the backend knows the identity, there is none"
        );
        assert_eq!(
            host_role(app.world()),
            None,
            "and no role is taken from that"
        );

        lobby_created(&mut app, 0xDEAD_BEEF);
        for _ in 0..3 {
            app.update();
        }
        let mut participants = app.world_mut().query::<&LobbyParticipant>();
        let roster_uuid = participants
            .iter(app.world())
            .find(|participant| participant.is_host)
            .expect("the host participant")
            .player_uuid;
        assert_eq!(
            roster_uuid, 0xDEAD_BEEF,
            "the roster carries the backend's uuid"
        );
        assert_eq!(
            host_role(app.world()),
            Some(roster_uuid),
            "and so does the role"
        );
    }
}
