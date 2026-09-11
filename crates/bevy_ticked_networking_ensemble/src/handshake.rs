//! Finding out at the join that two peers do not agree about what a snapshot means, and
//! letting nothing through until they do.
//!
//! # The failure this exists for
//!
//! A registry's sorted wire names **are** the wire format. Indices are a type's rank among
//! those names and travel in every snapshot, so a peer that registered `Health` where another
//! registered `WeaponState` reads one as the other. Postcard decodes it happily — the bytes are
//! the right length and the wrong meaning — so there is no error anywhere, just a world that
//! stops agreeing with itself in ways that look like everything except what they are.
//!
//! `TickedComponentRegistry::wire_hash` existed for that since the day registration names were
//! added, with a doc comment saying to exchange it at join time. **Nothing ever did.** Not this
//! crate, not either example, not either game. The number was computable and uncompared, so the
//! protection every consumer believed it had was a hand-kept discipline about never reordering
//! a registration, which is exactly the kind of rule that holds until somebody is in a hurry.
//!
//! # Gated, this time (audit finding F19)
//!
//! The first version of this file announced and compared, and tore the session down a few
//! frames after a mismatch. A few frames is enough for a snapshot to arrive and be applied: a
//! world built from bytes that mean something else, then despawned. So the comparison now
//! gates. A host sends snapshots only to a client whose registries it has matched
//! ([`TickedPeerVerified`], and with it the server's `SnapshotRecipientList`); a client
//! applies snapshots only once it has matched its host's ([`RegistryVerified`], checked by the
//! bridge before it forwards a packet). Role adoption is still keyed on
//! `LocalMultiplayerPlayerId`, the earliest moment a peer knows which body is its own, so
//! everything downstream of the local uuid is right from the start; what waits is the data.
//!
//! # Announced to a verified peer, once
//!
//! `bevy_ensemble` runs its own protocol handshake first and marks the counterpart
//! `HandshakeVerified` on a match. Ours is announced on that mark rather than on the entity
//! appearing, because until then the transport holds every packet from that peer anyway, and
//! because a peer whose *transport* protocol differs is refused by `bevy_ensemble` before this
//! crate gets a say.
//!
//! # It does not retry, and does not re-adopt
//!
//! A mismatch is not transient: the two builds differ and will still differ in a second. So
//! [`RegistryMismatch`] latches, blocks re-adoption, and clears only when the lobby goes —
//! otherwise `adopt_role` would take the role straight back and the pair would flap at frame
//! rate. A client that hears nothing at all within [`HandshakeTimeout`] latches
//! [`HandshakeTimedOut`] the same way: a session that never starts should say so, not sit
//! paused on `AwaitingSync` for ever.

use core::time::Duration;
use std::collections::BTreeMap;

use bevy::prelude::*;
use bevy_ensemble::prelude::*;
use bevy_ensemble::{HandshakeVerified, LobbyClientMessage, LobbyClientPlayerUuid};
use bevy_ticked::prelude::*;
use bevy_ticked::tick::CurrentTick;
use bevy_ticked_networking::client::LocalClientPlayer;
use bevy_ticked_networking::server::LocalServerPlayer;
use serde::{Deserialize, Serialize};

/// What each peer says about the shape of its registries.
///
/// Both index spaces, because they are independent: a peer can agree about every component and
/// still disagree about resources, and a check that compared one would validate whichever half
/// happened to change less.
///
/// The names travel alongside the hashes only to make the error useful. They prove nothing the
/// hashes do not — but "`Health` is registered here and not there" is a sentence somebody can
/// act on, where two 64-bit numbers that differ are not.
#[derive(Message, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TickedRegistryHandshake {
    pub components: u64,
    pub resources: u64,
    /// Sorted, as the wire has them.
    pub component_names: Vec<String>,
    pub resource_names: Vec<String>,
}

impl TickedRegistryHandshake {
    fn of(world: &World) -> Option<Self> {
        let components = world.get_resource::<TickedComponentRegistry>()?;
        let resources = world.get_resource::<TickedResourceRegistry>();
        Some(Self::from_registries(components, resources))
    }

    /// Reading the wire names freezes both registries: from the first announcement on, a late
    /// registration panics rather than silently renumbering what is already on the wire.
    fn from_registries(
        components: &TickedComponentRegistry,
        resources: Option<&TickedResourceRegistry>,
    ) -> Self {
        Self {
            components: components.wire_hash(),
            resources: resources.map_or(0, |registry| registry.wire_hash()),
            component_names: components.wire_names().map(str::to_owned).collect(),
            resource_names: resources
                .map(|registry| registry.wire_names().map(str::to_owned).collect())
                .unwrap_or_default(),
        }
    }

    fn matches(&self, other: &Self) -> bool {
        self.components == other.components && self.resources == other.resources
    }

    /// The first registration that differs between this build and the peer's, in words.
    ///
    /// Components before resources, "here and not there" before "there and not here", so the
    /// same pair of builds produces the same sentence on both machines up to which side is
    /// speaking.
    pub fn difference(&self, theirs: &Self) -> String {
        if let Some(sentence) =
            name_difference("component", &self.component_names, &theirs.component_names)
        {
            return sentence;
        }
        if let Some(sentence) =
            name_difference("resource", &self.resource_names, &theirs.resource_names)
        {
            return sentence;
        }
        format!(
            "the registries list the same names but hash differently (components {:#x} here, \
             {:#x} there; resources {:#x} here, {:#x} there): the protocol version differs",
            self.components, theirs.components, self.resources, theirs.resources
        )
    }
}

fn name_difference(kind: &str, ours: &[String], theirs: &[String]) -> Option<String> {
    if let Some(name) = ours.iter().find(|name| !theirs.contains(name)) {
        return Some(format!(
            "{kind} \"{name}\" is registered on this build and not on the peer's"
        ));
    }
    if let Some(name) = theirs.iter().find(|name| !ours.contains(name)) {
        return Some(format!(
            "the peer registers {kind} \"{name}\" which this build does not"
        ));
    }
    None
}

/// What a host tells a client whose registries matched. Reliable, host-only.
///
/// `slot` is the client's spawner slot, for the phase in which a client mints tracked ids in
/// its own range; `server_tick` says where the session's clock is; `send_every` is how many
/// ticks apart snapshots come (one, until the rate becomes negotiable).
#[derive(Message, Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TickedSessionWelcome {
    pub slot: u8,
    pub server_tick: u64,
    pub send_every: u64,
}

/// Set when a peer's registries turned out not to match this one's.
///
/// Its presence blocks [`adopt_role`](crate::session::adopt_role) — the session is over and must
/// not restart itself. Cleared when the lobby goes, so that joining a *different* lobby is allowed
/// to try again. `difference` is the sentence to show the player.
#[derive(Resource, Clone, Debug)]
pub struct RegistryMismatch {
    pub peer: u128,
    pub ours: TickedRegistryHandshake,
    pub theirs: TickedRegistryHandshake,
    /// The first registration that differs, in words. See
    /// [`TickedRegistryHandshake::difference`].
    pub difference: String,
}

/// Set on a client that waited [`HandshakeTimeout`] for its host's registries and never heard
/// them. Latches and blocks re-adoption like [`RegistryMismatch`]; cleared with the lobby.
#[derive(Resource, Clone, Copy, Debug, PartialEq, Eq)]
pub struct HandshakeTimedOut {
    pub waited: Duration,
}

/// How long a client waits for its host's registries before giving up. Inserted by
/// [`TickedEnsembleSessionPlugin`](crate::TickedEnsembleSessionPlugin) from its
/// `handshake_timeout`; a test overrides it by inserting its own.
#[derive(Resource, Clone, Copy, Debug, PartialEq, Eq)]
pub struct HandshakeTimeout(pub Duration);

/// On a client: the host's registries have been compared to this build's and match. Until it is
/// present the bridge drops every snapshot at the door.
#[derive(Resource, Clone, Copy, Debug, Default)]
pub struct RegistryVerified;

/// On a client: the spawner slot the host's [`TickedSessionWelcome`] assigned.
#[derive(Resource, Clone, Copy, Debug, PartialEq, Eq)]
pub struct LocalSpawnerSlot(pub u8);

/// On a host's `LobbyClient`: that client's registries match, so it is told the world.
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct TickedPeerVerified;

/// On a host: which spawner slot each verified client holds. Slot 0 is the host's own;
/// `1..=255` are handed out lowest-free-first and given back when the client's `LobbyClient`
/// is removed, so a rejoin can get its old number back and a 256th client is refused rather
/// than given somebody else's.
#[derive(Resource, Default, Debug, Clone)]
pub struct SpawnerSlots {
    by_uuid: BTreeMap<u128, u8>,
}

impl SpawnerSlots {
    /// The slot `uuid` holds, or the lowest free one from now on. `None` when all 255 are taken.
    pub fn assign(&mut self, uuid: u128) -> Option<u8> {
        if let Some(slot) = self.by_uuid.get(&uuid) {
            return Some(*slot);
        }
        let slot = (1..=u8::MAX).find(|slot| !self.by_uuid.values().any(|taken| taken == slot))?;
        self.by_uuid.insert(uuid, slot);
        Some(slot)
    }

    pub fn free(&mut self, uuid: u128) {
        self.by_uuid.remove(&uuid);
    }

    pub fn slot_of(&self, uuid: u128) -> Option<u8> {
        self.by_uuid.get(&uuid).copied()
    }

    /// `(uuid, slot)` for every client holding one, by uuid.
    pub fn iter(&self) -> impl Iterator<Item = (u128, u8)> + '_ {
        self.by_uuid.iter().map(|(uuid, slot)| (*uuid, *slot))
    }

    pub fn len(&self) -> usize {
        self.by_uuid.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_uuid.is_empty()
    }
}

/// Present on any app the handshake is installed in; what the bridge reads to know that
/// [`RegistryVerified`] is going to arrive and snapshots must wait for it.
#[derive(Resource, Clone, Copy, Debug, Default)]
pub struct HandshakeInstalled;

pub(crate) fn plugin(app: &mut App) {
    app.register_control_message_type::<TickedRegistryHandshake>(
        "bevy_ticked/RegistryHandshake",
        // Both sides announce: a host is told by every client, a client by its host.
        bevy_ensemble::MessageAuthority::Any,
    )
    .register_control_message_type::<TickedSessionWelcome>(
        "bevy_ticked/SessionWelcome",
        bevy_ensemble::MessageAuthority::HostOnly,
    )
    .init_resource::<SpawnerSlots>()
    .insert_resource(HandshakeInstalled)
    .add_observer(announce_registry)
    .add_observer(free_departed_slot)
    .add_systems(
        Update,
        (check_registry, receive_welcome, refuse_after_timeout).chain(),
    );
    if !app.world().contains_resource::<HandshakeTimeout>() {
        app.insert_resource(HandshakeTimeout(Duration::from_secs(5)));
    }
}

/// Tell a peer what our registries look like, the moment the transport has verified it.
///
/// # Per recipient, not per lobby
///
/// A host's lobby entity exists from the moment it starts hosting — before any client has
/// connected — so a single broadcast there goes to nobody, and every client that joins
/// afterwards is never told. The address is the entity `bevy_ensemble` marked: a `LobbyClient`
/// on the host, the lobby on a client. Both are per session, so leaving one and joining
/// another announces again — a new peer has been told nothing.
///
/// `SendMode::Reliable`: this is the one message in this crate that must not be dropped. An
/// unreliable handshake that goes missing is a session that never starts.
fn announce_registry(
    verified: On<Add, HandshakeVerified>,
    components: Option<Res<TickedComponentRegistry>>,
    resources: Option<Res<TickedResourceRegistry>>,
    clients: Query<(), With<LobbyClient>>,
    mut commands: Commands,
) {
    let Some(components) = components else {
        return;
    };
    let ours = TickedRegistryHandshake::from_registries(&components, resources.as_deref());
    let entity = verified.entity;
    if clients.contains(entity) {
        commands.entity(entity).trigger(move |entity| LobbyClientMessage {
            entity,
            message: ours,
            send_mode: SendMode::Reliable,
        });
    } else {
        commands.entity(entity).trigger(move |entity| LobbyMessage {
            entity,
            message: ours,
            send_mode: SendMode::Reliable,
        });
    }
}

/// Compare what arrived against what we hold; verify on a match, end the session otherwise.
///
/// On a host a mismatch refuses *that client*: it is never marked verified, so it never gets a
/// snapshot, and the error names the registration. The host's own session is untouched — three
/// players on the right build do not lose their game because a fourth is on the wrong one, and
/// the fourth finds out on its own side, since both peers compare. On a client a mismatch is
/// the end: the role is dropped, `reset_on_leave` clears whatever was built, and
/// [`RegistryMismatch`] keeps it from being taken back.
fn check_registry(world: &mut World) {
    let arrivals: Vec<(Option<u128>, TickedRegistryHandshake)> = {
        let mut messages =
            world.resource_mut::<Messages<ReceivedEnsembleMessage<TickedRegistryHandshake>>>();
        messages
            .drain()
            .map(|received| (received.sender, received.message))
            .collect()
    };
    if arrivals.is_empty() || world.contains_resource::<RegistryMismatch>() {
        return;
    }
    let Some(ours) = TickedRegistryHandshake::of(world) else {
        return;
    };
    let hosting = {
        let mut hosts = world.query_filtered::<(), (With<Host>, Or<(With<Lobby>, With<PendingLobby>)>)>();
        hosts.iter(world).next().is_some()
    };

    for (sender, theirs) in arrivals {
        let peer = sender.unwrap_or_default();
        if ours.matches(&theirs) {
            if hosting {
                verify_client(world, peer);
            } else {
                world.insert_resource(RegistryVerified);
            }
            continue;
        }
        let difference = ours.difference(&theirs);
        if hosting {
            error!(
                "refusing client {peer:#x}: its registries do not match this build's \
                 ({difference}). It will get no snapshot from this host. Build both peers from \
                 the same commit."
            );
            continue;
        }
        error!(
            "registry mismatch with the host {peer:#x}: {difference}. Every wire index from the \
             first difference onward means something else on the other machine, so the session \
             is being ended rather than played out. Build both peers from the same commit."
        );
        world.insert_resource(RegistryMismatch {
            peer,
            ours: ours.clone(),
            theirs,
            difference,
        });
        end_client_session(world);
        return;
    }
}

/// The host's side of a match: mark the client, give it a slot, tell it where the clock is.
fn verify_client(world: &mut World, uuid: u128) {
    let client = {
        let mut clients = world.query_filtered::<(Entity, &LobbyClientPlayerUuid), With<LobbyClient>>();
        clients
            .iter(world)
            .find(|(_, client)| client.0 == uuid)
            .map(|(entity, _)| entity)
    };
    let Some(client) = client else {
        // Verified and gone in the same frame. Nothing to mark; a rejoin announces again.
        return;
    };
    let Some(slot) = world.resource_mut::<SpawnerSlots>().assign(uuid) else {
        error!("refusing client {uuid:#x}: every spawner slot is taken");
        return;
    };
    world.entity_mut(client).insert(TickedPeerVerified);
    let server_tick = world.get_resource::<CurrentTick>().map_or(0, |tick| tick.0);
    let send_every = world
        .get_resource::<bevy_ticked_networking::server::SendEvery>()
        .map_or(1, |every| every.0);
    world.trigger(LobbyClientMessage {
        entity: client,
        message: TickedSessionWelcome {
            slot,
            server_tick,
            send_every,
        },
        send_mode: SendMode::Reliable,
    });
}

/// A client keeps the slot its host gave it, and draws remote bodies far enough behind the
/// host's send rate that there is always a next state to blend toward.
fn receive_welcome(
    mut welcomes: MessageReader<ReceivedEnsembleMessage<TickedSessionWelcome>>,
    mut commands: Commands,
) {
    for welcome in welcomes.read() {
        commands.insert_resource(LocalSpawnerSlot(welcome.message.slot));
        commands.insert_resource(bevy_ticked_networking::replication::InterpolationDelay(
            (2 * welcome.message.send_every).max(2),
        ));
    }
}

/// Give a slot back the moment its client's `LobbyClient` goes, whichever way it went.
fn free_departed_slot(
    remove: On<Remove, LobbyClient>,
    uuids: Query<&LobbyClientPlayerUuid>,
    mut slots: ResMut<SpawnerSlots>,
) {
    if let Ok(uuid) = uuids.get(remove.entity) {
        slots.free(uuid.0);
    }
}

/// A client that has held its role for [`HandshakeTimeout`] without hearing its host's
/// registries gives up, loudly.
///
/// Measured on `Time`, the frame clock, from the frame the client role was taken: it is the
/// player's wait that is being bounded, and a client whose transport never delivers the
/// reliable handshake is one whose transport is not going to deliver anything else either.
fn refuse_after_timeout(world: &mut World, mut waited: Local<Duration>) {
    let client = world.contains_resource::<LocalClientPlayer>();
    let verified = world.contains_resource::<RegistryVerified>();
    if !client || verified {
        *waited = Duration::ZERO;
        return;
    }
    *waited += world.resource::<Time>().delta();
    let timeout = world
        .get_resource::<HandshakeTimeout>()
        .map_or(Duration::from_secs(5), |timeout| timeout.0);
    if *waited < timeout {
        return;
    }
    let waited_for = *waited;
    *waited = Duration::ZERO;
    error!(
        "the host never announced its registries in {waited_for:?}: the registry handshake did \
         not complete, so no snapshot has been applied and the session is being ended. The host \
         is on a build without the handshake, or the link never delivered it."
    );
    world.insert_resource(HandshakeTimedOut { waited: waited_for });
    end_client_session(world);
}

/// Dropping the roles is what actually ends it: `reset_on_leave` fires on their removal and
/// clears the queue, the tick and every tracked entity the wrong-shaped snapshots built.
fn end_client_session(world: &mut World) {
    world.remove_resource::<LocalServerPlayer>();
    world.remove_resource::<LocalClientPlayer>();
    world.remove_resource::<RegistryVerified>();
    world.remove_resource::<LocalSpawnerSlot>();
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy_ticked::registry::TickedComponentRegistry;
    use bevy_ticked_networking::networked_registry::NetworkedTickedAppExt;

    #[derive(Component, Clone, Copy, Serialize, Deserialize)]
    struct Pos(i32);

    /// A peer mid-session, holding a client role and a one-entry registry.
    fn joined_client() -> App {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins)
            .add_plugins(EnsemblePlugin)
            .init_resource::<TickedComponentRegistry>()
            .register_networked_ticked_component::<Pos>("Pos")
            .add_plugins(plugin);
        app.world_mut().insert_resource(LocalClientPlayer(7));
        app.world_mut().spawn(Lobby);
        app.update();
        app
    }

    fn deliver(app: &mut App, message: TickedRegistryHandshake) {
        app.world_mut().write_message(ReceivedEnsembleMessage {
            sender: Some(9),
            message,
            received_at: bevy_ensemble::Instant::now(),
        });
        app.update();
    }

    fn ours(app: &App) -> TickedRegistryHandshake {
        TickedRegistryHandshake::of(app.world()).expect("a registry")
    }

    fn handshake(components: &[&str], resources: &[&str]) -> TickedRegistryHandshake {
        TickedRegistryHandshake {
            components: components.len() as u64,
            resources: resources.len() as u64,
            component_names: components.iter().map(|name| (*name).to_owned()).collect(),
            resource_names: resources.iter().map(|name| (*name).to_owned()).collect(),
        }
    }

    #[test]
    fn a_peer_with_the_same_registries_is_verified() {
        let mut app = joined_client();
        let same = ours(&app);
        deliver(&mut app, same);

        assert!(
            app.world().get_resource::<RegistryMismatch>().is_none(),
            "agreement is the common case and must cost nothing"
        );
        assert!(
            app.world().get_resource::<LocalClientPlayer>().is_some(),
            "and must not disturb the session"
        );
        assert!(
            app.world().contains_resource::<RegistryVerified>(),
            "a match is what opens the gate"
        );
    }

    #[test]
    fn a_peer_with_a_different_component_registry_ends_the_session() {
        let mut app = joined_client();
        let mut theirs = ours(&app);
        theirs.components ^= 1;
        theirs.component_names.push("Health".to_owned());
        deliver(&mut app, theirs);

        let mismatch = app
            .world()
            .get_resource::<RegistryMismatch>()
            .expect("latched");
        assert_eq!(
            mismatch.difference,
            "the peer registers component \"Health\" which this build does not"
        );
        assert!(
            app.world().get_resource::<LocalClientPlayer>().is_none(),
            "the role is dropped, which is what makes reset_on_leave clear the wrong-shaped world"
        );
        assert!(!app.world().contains_resource::<RegistryVerified>());
    }

    /// The half a component-only check would have missed, and the reason the resource hash is a
    /// separate number on the wire rather than folded into one.
    #[test]
    fn a_peer_with_a_different_resource_registry_ends_the_session() {
        let mut app = joined_client();
        let mut theirs = ours(&app);
        theirs.resources ^= 1;
        deliver(&mut app, theirs);

        assert!(app.world().get_resource::<RegistryMismatch>().is_some());
        assert!(app.world().get_resource::<LocalClientPlayer>().is_none());
    }

    /// A mismatch latches. Without this the role is dropped, `adopt_role` takes it straight back
    /// on the next frame because the lobby is still there, and the pair flap at frame rate.
    #[test]
    fn a_mismatch_is_not_retried_every_frame() {
        let mut app = joined_client();
        let mut theirs = ours(&app);
        theirs.components ^= 1;
        deliver(&mut app, theirs);

        for _ in 0..8 {
            app.update();
        }
        assert!(
            app.world().get_resource::<RegistryMismatch>().is_some(),
            "the two builds still differ eight frames later"
        );
        assert!(app.world().get_resource::<LocalClientPlayer>().is_none());
    }

    #[test]
    fn the_difference_names_the_first_registration_that_differs() {
        let ours = handshake(&["Health", "Pos"], &["Round"]);
        let theirs = handshake(&["Pos", "WeaponState"], &["Round"]);
        assert_eq!(
            ours.difference(&theirs),
            "component \"Health\" is registered on this build and not on the peer's"
        );
        assert_eq!(
            theirs.difference(&ours),
            "component \"WeaponState\" is registered on this build and not on the peer's"
        );
        let same_components = handshake(&["Pos"], &["Round"]);
        let extra_resource = handshake(&["Pos"], &["Round", "Score"]);
        assert_eq!(
            same_components.difference(&extra_resource),
            "the peer registers resource \"Score\" which this build does not"
        );
    }

    #[test]
    fn a_version_difference_is_named_when_the_names_agree() {
        let ours = handshake(&["Pos"], &[]);
        let mut theirs = ours.clone();
        theirs.components ^= 1;
        assert!(ours.difference(&theirs).contains("protocol version"));
    }

    #[test]
    fn slots_are_handed_out_lowest_free_first_and_given_back() {
        let mut slots = SpawnerSlots::default();
        assert_eq!(slots.assign(10), Some(1));
        assert_eq!(slots.assign(11), Some(2));
        assert_eq!(slots.assign(10), Some(1), "asking twice is the same slot");
        slots.free(10);
        assert_eq!(slots.assign(12), Some(1), "a freed slot is reused before a new one");
        assert_eq!(slots.slot_of(11), Some(2));
        assert_eq!(slots.slot_of(10), None);
    }

    #[test]
    fn the_two_hundred_and_fifty_sixth_client_is_refused() {
        let mut slots = SpawnerSlots::default();
        for uuid in 0..255u128 {
            assert!(slots.assign(uuid + 1000).is_some());
        }
        assert_eq!(slots.assign(5000), None);
    }

    /// A client that never hears from its host does not wait for ever.
    #[test]
    fn a_silent_host_is_given_up_on_after_the_timeout() {
        let mut app = joined_client();
        app.insert_resource(HandshakeTimeout(Duration::from_millis(50)));
        app.insert_resource(bevy::time::TimeUpdateStrategy::ManualDuration(
            Duration::from_millis(10),
        ));
        // The first frame of a manual clock has no delta, so four updates are 30 ms.
        for _ in 0..4 {
            app.update();
        }
        assert!(
            app.world().get_resource::<HandshakeTimedOut>().is_none(),
            "not yet: 30 ms of a 50 ms timeout"
        );
        for _ in 0..3 {
            app.update();
        }
        assert!(app.world().get_resource::<HandshakeTimedOut>().is_some());
        assert!(app.world().get_resource::<LocalClientPlayer>().is_none());
    }
}
