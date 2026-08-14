//! Becoming a host, becoming a client, and stopping being either.
//!
//! # Why this belongs here and not in the game
//!
//! `bevy_ticked_networking` has two role resources; `bevy_ensemble` has lobbies, participants and
//! a local uuid. Mapping the second onto the first is the one thing that turns two independent
//! crates into a session, and until now it was left to the consumer — so **every** consumer wrote
//! it, including both examples in this repository, and they wrote it differently in the places
//! that are hardest to get right.
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
//! - **What to tear down.** Upstream's `reset_on_host` raises the entity counter and
//!   `reset_on_join` zeroes it, but neither despawns what the peer built while it thought it was
//!   alone. A solo body left standing is an untracked duplicate the instant the host's world
//!   arrives — the host's copy of that player spawns beside it and both are drawn.
//!
//! - **The window in between.** Between a lobby appearing and a role being adopted, a peer is
//!   still nominally the authority over its own solo world *and* the participant roster has
//!   already arrived. Anything that mints a tracked entity there is doing a client-side spawn: the
//!   next snapshot despawns it, the system spawns it again, and the two chase each other at the
//!   snapshot rate. [`may_spawn_tracked`] is the guard, and it is strictly stricter than "am I the
//!   authority".
//!
//! # Opt in, rather than automatic
//!
//! [`TickedEnsembleSessionPlugin`] is separate from
//! [`TickedNetworkingEnsemblePlugin`](crate::TickedNetworkingEnsemblePlugin) on purpose. A
//! consumer that already adopts roles by hand would otherwise find this crate doing it too — and
//! the despawn above is not something to start doing to somebody's world without being asked.

use bevy::prelude::*;
use bevy_ensemble::prelude::*;
use bevy_ticked::prelude::*;
use bevy_ticked_networking::client::LocalClientPlayer;
use bevy_ticked_networking::server::{LocalServerPlayer, SnapshotRecipients};

/// Adopt and release the ticked role from the ensemble lobby, and keep
/// [`SnapshotRecipients`] current.
///
/// Add alongside [`TickedNetworkingEnsemblePlugin`](crate::TickedNetworkingEnsemblePlugin) to stop
/// writing session bookkeeping by hand.
pub struct TickedEnsembleSessionPlugin;

impl Plugin for TickedEnsembleSessionPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<SnapshotRecipients>().add_systems(
            Update,
            (adopt_role, release_role, count_recipients).chain(),
        );
    }
}

/// True when this peer holds neither role — playing alone, or not in a session yet.
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

/// Take the host or client role as soon as this peer knows which body is its own.
fn adopt_role(
    mut commands: Commands,
    local_player: Option<Res<LocalMultiplayerPlayerId>>,
    server: Option<Res<LocalServerPlayer>>,
    client: Option<Res<LocalClientPlayer>>,
    tracked: Query<Entity, With<TickTrackedEntity>>,
    hosting: Query<(), (With<Host>, Or<(With<Lobby>, With<PendingLobby>)>)>,
    joined: Query<(), (Without<Host>, Or<(With<Lobby>, With<PendingLobby>)>)>,
) {
    if server.is_some() || client.is_some() {
        return;
    }
    let Some(local_player) = local_player else {
        return;
    };
    let is_host = !hosting.is_empty();
    if !is_host && joined.is_empty() {
        return;
    }

    // The solo world ends here and it has to end completely. Upstream's resets zero the tick, the
    // counter and the history; neither despawns, and a body left standing is an untracked
    // duplicate the moment the host's world arrives.
    for entity in &tracked {
        commands.entity(entity).try_despawn();
    }

    if is_host {
        commands.insert_resource(LocalServerPlayer(local_player.0));
        // A host's clock is the session's clock, so there is nothing to wait for.
        commands.remove_resource::<TicksPaused>();
    } else {
        // `TicksPaused` stays: `TickedClientPlugin` lifts it once there is a tick to sync to.
        commands.insert_resource(LocalClientPlayer(local_player.0));
    }
}

/// Give the role back when the lobby goes, however it went.
///
/// A state check rather than `RemovedComponents<Lobby>`, because a refused join despawns an entity
/// that never carried [`Lobby`] at all — the removal never fires, and the peer sits in a session
/// that does not exist.
fn release_role(
    mut commands: Commands,
    server: Option<Res<LocalServerPlayer>>,
    client: Option<Res<LocalClientPlayer>>,
    lobbies: Query<(), Or<(With<Lobby>, With<PendingLobby>)>>,
) {
    if server.is_none() && client.is_none() {
        return;
    }
    if !lobbies.is_empty() {
        return;
    }
    // `reset_on_leave` does the rest, keyed off these being removed.
    commands.remove_resource::<LocalServerPlayer>();
    commands.remove_resource::<LocalClientPlayer>();
}

/// How many peers a snapshot would reach, for [`SnapshotRecipients`].
fn count_recipients(
    mut recipients: ResMut<SnapshotRecipients>,
    clients: Query<(), With<LobbyClient>>,
) {
    let count = clients.iter().count();
    if recipients.0 != count {
        recipients.0 = count;
    }
}
