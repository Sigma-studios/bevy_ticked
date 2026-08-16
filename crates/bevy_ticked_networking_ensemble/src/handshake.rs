//! Finding out at the join that two peers do not agree about what a snapshot means.
//!
//! # The failure this exists for
//!
//! A registry's order **is** a wire format. Indices are assigned by position and travel in every
//! snapshot, so a peer that registered `Health` where another registered `WeaponState` reads one
//! as the other. Postcard decodes it happily — the bytes are the right length and the wrong
//! meaning — so there is no error anywhere, just a world that stops agreeing with itself in ways
//! that look like everything except what they are.
//!
//! [`TickedComponentRegistry::wire_hash`] has existed for that since the day registration names
//! were added, with a doc comment saying to exchange it at join time. **Nothing ever did.** Not
//! this crate, not either example, not either game. The number was computable and uncompared, so
//! the protection every consumer believed it had was a hand-kept discipline about never reordering
//! a registration, which is exactly the kind of rule that holds until somebody is in a hurry.
//!
//! # Verified rather than gated, and why
//!
//! The obvious design is to refuse to adopt a role until the peer's hash has arrived and matched.
//! It is the wrong one here. Adoption is deliberately keyed on `LocalMultiplayerPlayerId` — the
//! earliest moment a peer knows which body is its own, and *earlier than any data channel exists*
//! — so gating on a message that necessarily arrives later would give up the property
//! `session::adopt_role` was written to have, and would hang the session for ever if the message
//! were lost.
//!
//! So the session starts, both peers announce, and a mismatch tears it down within a few frames.
//! "An hour later" was the complaint; a handful of frames is not that.
//!
//! # It does not retry, and does not re-adopt
//!
//! A mismatch is not transient: the two builds differ and will still differ in a second. So
//! [`RegistryMismatch`] latches, blocks re-adoption, and clears only when the peer leaves the
//! lobby entirely — otherwise `adopt_role` would take the role straight back and the pair would
//! flap at frame rate.

use bevy::platform::collections::HashSet;
use bevy::prelude::*;
use bevy_ensemble::prelude::*;
use bevy_ticked::prelude::*;
use bevy_ticked_networking::client::LocalClientPlayer;
use bevy_ticked_networking::server::LocalServerPlayer;
use serde::{Deserialize, Serialize};

/// What each peer says about the shape of its registries.
///
/// Both index spaces, because they are independent: a peer can agree about every component and
/// still disagree about resources, and a check that compared one would validate whichever half
/// happened to change less.
///
/// The counts travel alongside the hashes only to make the log line useful. They prove nothing the
/// hashes do not — but "yours has 15 components, mine has 14" is a sentence somebody can act on,
/// where two 64-bit numbers that differ are not.
#[derive(Message, Clone, Copy, Debug, Serialize, Deserialize)]
pub struct TickedRegistryHandshake {
    pub components: u64,
    pub resources: u64,
    pub component_count: u16,
    pub resource_count: u16,
}

impl TickedRegistryHandshake {
    fn of(world: &World) -> Option<Self> {
        let components = world.get_resource::<TickedComponentRegistry>()?;
        let resources = world.get_resource::<TickedResourceRegistry>();
        Some(Self {
            components: components.wire_hash(),
            resources: resources.map_or(0, |registry| registry.wire_hash()),
            component_count: components.len() as u16,
            resource_count: resources.map_or(0, |registry| registry.len() as u16),
        })
    }
}

/// Set when a peer's registries turned out not to match this one's.
///
/// Its presence blocks [`adopt_role`](crate::session::adopt_role) — the session is over and must
/// not restart itself. Cleared when the lobby goes, so that joining a *different* lobby is allowed
/// to try again.
#[derive(Resource, Clone, Copy, Debug)]
pub struct RegistryMismatch {
    pub peer: u128,
    pub ours: TickedRegistryHandshake,
    pub theirs: TickedRegistryHandshake,
}

pub(crate) fn plugin(app: &mut App) {
    app.register_ensemble_message_type::<TickedRegistryHandshake>()
        .add_systems(Update, (announce_registry, check_registry).chain());
}

/// Tell each peer what our registries look like, once each.
///
/// # Once per *recipient*, not once per lobby
///
/// The obvious version sends to the lobby entity the first time one exists, and it is wrong on a
/// host in a way that testing against a matching pair cannot show. A host's lobby entity exists
/// from the moment it starts hosting — before any client has connected — so a single broadcast
/// there goes to nobody, and every client that joins afterwards is never told. Only the
/// client→host direction worked, which was enough to *detect* a mismatch (the host always receives)
/// but left the client sitting in a session that had quietly ended, with no snapshots and no
/// reason given.
///
/// So the address is the recipient: a client tells the lobby, which is the host; a host tells each
/// [`LobbyClient`] as it appears. `told` is keyed by entity so that leaving one session and joining
/// another announces again — a new peer has been told nothing.
///
/// `SendMode::Reliable`: this is the one message in this crate that must not be dropped. An
/// unreliable handshake that goes missing is a session that silently keeps the protection it was
/// supposed to have proved.
fn announce_registry(world: &mut World, mut told: Local<HashSet<Entity>>) {
    let hosting = {
        let mut hosts = world.query_filtered::<Entity, (With<Lobby>, With<Host>)>();
        hosts.iter(world).next().is_some()
    };

    let recipients: Vec<Entity> = if hosting {
        let mut clients = world.query_filtered::<Entity, With<LobbyClient>>();
        clients.iter(world).collect()
    } else {
        let mut lobbies = world.query_filtered::<Entity, With<Lobby>>();
        lobbies.iter(world).collect()
    };

    // Forget peers that have gone, so a reused entity id is told again rather than assumed told.
    told.retain(|entity| recipients.contains(entity));

    let fresh: Vec<Entity> = recipients
        .into_iter()
        .filter(|entity| !told.contains(entity))
        .collect();
    if fresh.is_empty() {
        return;
    }
    let Some(ours) = TickedRegistryHandshake::of(world) else {
        return;
    };
    for entity in fresh {
        told.insert(entity);
        world.commands().entity(entity).trigger(move |entity| LobbyMessage {
            entity,
            message: ours,
            send_mode: SendMode::Reliable,
        });
    }
}

/// Compare what arrived against what we hold, and end the session if they differ.
fn check_registry(world: &mut World) {
    let arrivals: Vec<(Option<u128>, TickedRegistryHandshake)> = {
        let mut messages = world
            .resource_mut::<Messages<ReceivedEnsembleMessage<TickedRegistryHandshake>>>();
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

    for (sender, theirs) in arrivals {
        if theirs.components == ours.components && theirs.resources == ours.resources {
            continue;
        }
        let peer = sender.unwrap_or_default();
        error!(
            "registry mismatch with peer {peer:#x}: this build registers {} components / {} \
             resources (hashes {:#x} / {:#x}), that one registers {} / {} ({:#x} / {:#x}). \
             Every component index from the first difference onward means something else on the \
             other machine, so the session is being ended rather than played out. Build both \
             peers from the same commit.",
            ours.component_count,
            ours.resource_count,
            ours.components,
            ours.resources,
            theirs.component_count,
            theirs.resource_count,
            theirs.components,
            theirs.resources,
        );
        world.insert_resource(RegistryMismatch { peer, ours, theirs });
        // Dropping the roles is what actually ends it: `reset_on_leave` fires on their removal and
        // clears the queue, the tick and every tracked entity the wrong-shaped snapshots built.
        world.remove_resource::<LocalServerPlayer>();
        world.remove_resource::<LocalClientPlayer>();
        return;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy_ticked::registry::TickedComponentRegistry;

    #[derive(Component, Clone, Copy)]
    struct Pos(i32);

    /// A peer mid-session, holding a client role and a one-entry registry.
    fn joined_client() -> App {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins)
            .add_plugins(EnsemblePlugin)
            .init_resource::<TickedComponentRegistry>()
            .register_ticked_component_as::<Pos>("Pos")
            .add_plugins(plugin);
        app.world_mut().insert_resource(LocalClientPlayer(7));
        app.update();
        app
    }

    fn deliver(app: &mut App, message: TickedRegistryHandshake) {
        app.world_mut()
            .write_message(ReceivedEnsembleMessage {
                sender: Some(9),
                message,
                received_at: std::time::Duration::ZERO,
            });
        app.update();
    }

    fn ours(app: &App) -> TickedRegistryHandshake {
        TickedRegistryHandshake::of(app.world()).expect("a registry")
    }

    #[test]
    fn a_peer_with_the_same_registries_is_left_alone() {
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
    }

    #[test]
    fn a_peer_with_a_different_component_registry_ends_the_session() {
        let mut app = joined_client();
        let mut theirs = ours(&app);
        theirs.components ^= 1;
        deliver(&mut app, theirs);

        assert!(app.world().get_resource::<RegistryMismatch>().is_some());
        assert!(
            app.world().get_resource::<LocalClientPlayer>().is_none(),
            "the role is dropped, which is what makes reset_on_leave clear the wrong-shaped world"
        );
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
}
