//! One shell, several windows: a whole local session from a single `cargo run`.
//!
//! ```text
//! TICKED_LOCAL_SESSION=2 cargo run --example fps_shooter
//! ```
//!
//! The process that reads `TICKED_LOCAL_SESSION=N` is the launcher: it starts a signalling
//! server on a free port in-process, starts `N - 1` more copies of its own executable with
//! `TICKED_ROLE=client` and `SIGNALLING_SERVER_URL` pointing at that server, and hosts. Every
//! copy joins the first lobby the server lists. Closing the launcher's window ends the
//! children. Nothing else in the game changes: a build without the variables behaves as
//! before, and `TICKED_ROLE=host|client` alone drives a single process against a server named
//! by `SIGNALLING_SERVER_URL`.
//!
//! Two lines in a game:
//!
//! ```ignore
//! .add_plugins(BevyEnsembleWebrtcPlugin { server_url: local_session::signalling_url(), ..default() })
//! .add_plugins(TickedLocalSessionPlugin)
//! ```
//!
//! [`signalling_url`] has to be the one that decides the URL, because the launcher's server
//! exists only once it is asked for and the WebRTC plugin takes the URL by value.

use std::process::{Child, Command};
use std::sync::OnceLock;

use bevy::prelude::*;
use bevy_ensemble::{Lobby, PendingLobby, PublicLobbies, StartHosting};
use bevy_ensemble_webrtc::server::test_support::SignallingServer;
use bevy_ensemble_webrtc::{JoinWebrtcLobby, RefreshLobbyList};

/// `TICKED_LOCAL_SESSION=N`: how many peers the launcher runs, itself included.
pub const PEERS_VAR: &str = "TICKED_LOCAL_SESSION";
/// `TICKED_ROLE=host|client`: what this process does with the lobby list on its own.
pub const ROLE_VAR: &str = "TICKED_ROLE";
/// The signalling server every process connects to.
pub const URL_VAR: &str = "SIGNALLING_SERVER_URL";
const DEFAULT_URL: &str = "ws://localhost:9090/ws";

static LAUNCHER_SERVER: OnceLock<SignallingServer> = OnceLock::new();

/// Whether this process is the launcher: it read the peer count and was not itself launched.
fn is_launcher() -> bool {
    peer_count().is_some() && std::env::var(ROLE_VAR).is_err()
}

fn peer_count() -> Option<u32> {
    std::env::var(PEERS_VAR)
        .ok()?
        .parse()
        .ok()
        .filter(|n| *n >= 1)
}

/// The signalling server URL for this process.
///
/// `SIGNALLING_SERVER_URL` when set; the launcher's own in-process server when this process
/// is the launcher (started on first call, on a free port); the local default otherwise.
pub fn signalling_url() -> String {
    if let Ok(url) = std::env::var(URL_VAR) {
        return url;
    }
    if is_launcher() {
        return LAUNCHER_SERVER
            .get_or_init(SignallingServer::start)
            .ws_url();
    }
    DEFAULT_URL.to_owned()
}

/// What this process does about a lobby on its own.
#[derive(Resource, Debug)]
enum AutoRole {
    Host,
    Client { last_refresh: f32, joined: bool },
}

/// The processes the launcher started; ended when the launcher's world is dropped.
#[derive(Resource, Default)]
struct Children(Vec<Child>);

impl Drop for Children {
    fn drop(&mut self) {
        for child in &mut self.0 {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Hosts or joins from `TICKED_ROLE`; as the launcher, starts the other peers first.
pub struct TickedLocalSessionPlugin;

impl Plugin for TickedLocalSessionPlugin {
    fn build(&self, app: &mut App) {
        let role = match std::env::var(ROLE_VAR).ok().as_deref() {
            Some("host") => Some(AutoRole::Host),
            Some("client") => Some(AutoRole::Client {
                last_refresh: 0.0,
                joined: false,
            }),
            Some(other) => {
                warn!("{ROLE_VAR}={other:?} is not `host` or `client`; ignoring it");
                None
            }
            None if is_launcher() => Some(AutoRole::Host),
            None => None,
        };
        if is_launcher() {
            let peers = peer_count().unwrap_or(1);
            let url = signalling_url();
            let mut children = Children::default();
            match std::env::current_exe() {
                Ok(exe) => {
                    for index in 1..peers {
                        match Command::new(&exe)
                            .args(std::env::args().skip(1))
                            .env(ROLE_VAR, "client")
                            .env(URL_VAR, &url)
                            .env_remove(PEERS_VAR)
                            .spawn()
                        {
                            Ok(child) => {
                                info!(
                                    "local session: started peer {index} of {peers} (pid {})",
                                    child.id()
                                );
                                children.0.push(child);
                            }
                            Err(error) => {
                                error!("local session: could not start peer {index}: {error}")
                            }
                        }
                    }
                }
                Err(error) => {
                    error!("local session: cannot find this executable to start peers: {error}")
                }
            }
            info!("local session: hosting on {url}");
            app.insert_resource(children);
        }
        if let Some(role) = role {
            app.insert_resource(role);
        }
        app.add_systems(Update, drive_auto_role);
    }
}

fn drive_auto_role(
    auto: Option<ResMut<AutoRole>>,
    time: Res<Time<Real>>,
    lobbies: Query<(), Or<(With<Lobby>, With<PendingLobby>)>>,
    listed: Option<Res<PublicLobbies>>,
    mut start_hosting: MessageWriter<StartHosting>,
    mut refresh: MessageWriter<RefreshLobbyList>,
    mut join: MessageWriter<JoinWebrtcLobby>,
) {
    let Some(mut auto) = auto else { return };
    if !lobbies.is_empty() {
        return;
    }
    match &mut *auto {
        AutoRole::Host => {
            start_hosting.write(StartHosting);
            // Once: a lobby that ends (the player left it) is not re-hosted behind their back.
            *auto = AutoRole::Client {
                last_refresh: f32::MAX,
                joined: true,
            };
        }
        AutoRole::Client { joined: true, .. } => {}
        AutoRole::Client {
            last_refresh,
            joined,
        } => {
            if let Some(first) = listed.as_ref().and_then(|list| list.0.first()) {
                join.write(JoinWebrtcLobby(first.lobby_id));
                *joined = true;
                return;
            }
            let now = time.elapsed_secs();
            if now - *last_refresh >= 1.0 || *last_refresh == 0.0 {
                refresh.write(RefreshLobbyList);
                *last_refresh = now;
            }
        }
    }
}
