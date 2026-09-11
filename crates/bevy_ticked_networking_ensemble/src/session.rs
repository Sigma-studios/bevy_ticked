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
//!   can exist. Earlier still is the placeholder a host holds between asking to host and the
//!   lobby being created, and that one is *not* adopted from: see [`adopt_role`].
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

use core::time::Duration;

use bevy::prelude::*;
use bevy_ensemble::prelude::*;
// `PeerRtt` / `PeerRttJitter` come in via the prelude above; named here so the reason they are
// wanted is legible at the import site.
use bevy_ensemble::{PeerRtt, PeerRttJitter};
use bevy_ticked::prelude::*;
use bevy_ticked::time::{Ticked, TickedTime};
use bevy_ticked_networking::client::{ClientTickBuffer, LocalClientPlayer};
use bevy_ticked_networking::server::{LocalServerPlayer, SnapshotRecipients};

use crate::handshake::RegistryMismatch;

/// Adopt and release the ticked role from the ensemble lobby, and keep
/// [`SnapshotRecipients`] current.
///
/// Add alongside [`TickedNetworkingEnsemblePlugin`](crate::TickedNetworkingEnsemblePlugin) to stop
/// writing session bookkeeping by hand.
pub struct TickedEnsembleSessionPlugin;

impl Plugin for TickedEnsembleSessionPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<SnapshotRecipients>()
            .add_plugins(crate::handshake::plugin)
            .add_systems(
                Update,
                (
                    adopt_role,
                    seed_tick_buffer,
                    release_role,
                    forget_mismatch,
                    count_recipients,
                )
                    .chain(),
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
    mismatch: Option<Res<RegistryMismatch>>,
    tracked: Query<Entity, With<TickTrackedEntity>>,
    hosting: Query<(), (With<Host>, Or<(With<Lobby>, With<PendingLobby>)>)>,
    joined: Query<(), (Without<Host>, Or<(With<Lobby>, With<PendingLobby>)>)>,
) {
    if server.is_some() || client.is_some() {
        return;
    }
    // A session this peer cannot speak the language of does not get retried at frame rate.
    if mismatch.is_some() {
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
/// Only while the client is still waiting for its first snapshot, which is exactly the span
/// [`TicksPaused`] covers on a client: after that the buffer holds measurements, and a seed is
/// a guess that would be overwriting them. Pings start on the first frame a lobby exists, so a
/// sample is usually there in time — and when it isn't, nothing happens and the default is used.
/// That is survivable rather than free: it costs one correction shortly after the join.
fn seed_tick_buffer(
    client: Option<Res<LocalClientPlayer>>,
    paused: Option<Res<TicksPaused>>,
    ticked: Res<Time<Ticked>>,
    connection: Query<(&PeerRtt, Option<&PeerRttJitter>), With<Lobby>>,
    mut buffer: ResMut<ClientTickBuffer>,
    mut seeded: Local<bool>,
) {
    if client.is_none() {
        // Not a client, or no longer one: the next join gets a fresh seed.
        *seeded = false;
        return;
    }
    if *seeded || paused.is_none() {
        return;
    }
    let Some((rtt, jitter)) = connection.iter().next() else {
        // No ping has come back yet. Try again next frame, until the first snapshot closes the
        // window.
        return;
    };
    *seeded = true;
    buffer.seed_from_rtt(
        Duration::from_secs_f64(rtt.0.max(0.0)),
        Duration::from_secs_f64(jitter.map_or(0.0, |jitter| jitter.0.max(0.0))),
        ticked.timestep(),
    );
    debug!(
        "sized the prediction buffer from the link: rtt {:.0}ms, jitter {:.0}ms -> \
         replay distance {} ticks, margin {} ticks",
        rtt.0 * 1000.0,
        jitter.map_or(0.0, |jitter| jitter.0) * 1000.0,
        buffer.target_replay_distance,
        buffer.target_margin,
    );
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

/// Forget a registry mismatch once the lobby it belonged to is gone.
///
/// Joining a *different* lobby is allowed to try again — the peer on the other end of that one may
/// well have been built from the same commit as this.
fn forget_mismatch(
    mut commands: Commands,
    mismatch: Option<Res<RegistryMismatch>>,
    lobbies: Query<(), Or<(With<Lobby>, With<PendingLobby>)>>,
) {
    if mismatch.is_some() && lobbies.is_empty() {
        commands.remove_resource::<RegistryMismatch>();
    }
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
            .add_plugins(TickedEnsembleSessionPlugin);
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
            app.world().get_resource::<LocalMultiplayerPlayerId>().is_none(),
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
