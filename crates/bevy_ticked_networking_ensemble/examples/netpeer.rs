//! One peer of a real session, in its own process, with no window.
//!
//! The in-process harness (`bevy_ticked_testing`) proves the simulation agrees when packets are
//! handed between peers by a `Vec`. This proves the rest: a real signalling handshake, a real
//! WebRTC data channel, real serialization, two OS processes with independent clocks. What
//! the loopback backend papers over shows up here.
//!
//! Driven by arguments so `tests/webrtc_multiprocess.rs` and `scripts/netpeers.sh` can spawn
//! several and compare what they wrote:
//!
//! ```text
//! netpeer --role host   --ticks 400 --out host.checksums
//! netpeer --role client --index 0 --ticks 400 --out client-0.checksums
//! ```
//!
//! Each peer writes one line per tick — `<tick> <hash> <bodies> <positions> <velocities>` —
//! so a mismatch names the tick it started at and the section that moved. It logs
//! `LOG_SESSION_START` once the session is up (a host: its first verified client; a client:
//! its first applied snapshot), which is where the scripts start counting warnings.
//!
//! | flag | |
//! | --- | --- |
//! | `--role host\|client` | required; a client joins the only lobby the server lists |
//! | `--index N` | which client this is (default 0) |
//! | `--ticks N` | run to this tick, then linger and leave (default 400) |
//! | `--out PATH` | where the checksum log goes (default: none) |
//! | `--linger MS` | keep ticking this long after the target (default 2000) |
//! | `--timeout SECS` | give up rather than hang (default 60) |
//! | `--walk` | hold a direction, alternating every 64 ticks |
//! | `--fire` | fire a pellet every 32 ticks |
//!
//! `SIGNALLING_SERVER_URL` picks the server (default `ws://localhost:9090/ws`).
//!
//! Exit codes: 0 the session ran to the target; 1 it did not; 2 bad arguments; 3 the transport
//! never connected within the timeout (inconclusive: nothing about the game was tested).

use std::fs::File;
use std::io::Write as _;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use bevy::prelude::*;
use bevy::time::TimeUpdateStrategy;
use bevy_ensemble::{
    EnsemblePlugin, Host, Lobby, LobbyClient, LobbyLeft, LobbyParticipant, LobbyParticipantOf,
    LocalMultiplayerPlayerId, PendingLobby, PublicLobbies, StartHosting,
};

use bevy_ensemble_webrtc::{BevyEnsembleWebrtcPlugin, JoinWebrtcLobby, RefreshLobbyList};
use bevy_ticked::checksum::ChecksumLog;
use bevy_ticked::prelude::*;
use bevy_ticked::tick::CurrentTick;
use bevy_ticked::tracked_entity::TrackedIdAllocator;
use bevy_ticked_networking::client::LocalClientPlayer;
use bevy_ticked_networking::diagnostics::ReplayStats;
use bevy_ticked_networking::prelude::*;
use bevy_ticked_networking_ensemble::{
    SpawnerSlots, TickedEnsembleSessionPlugin, TickedNetworkingEnsemblePlugin, TickedPeerVerified,
};
use bevy_ticked_testing::fixtures::minimal::{self, Input, MinimalHash, PlayerSlot, Pos, Vel};
use bevy_ticked_testing::fixtures::minimal::{EntityKind, Owner};
use bevy_ticked_testing::peer::TICK;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Role {
    Host,
    Client,
}

#[derive(Resource, Clone)]
struct PeerConfig {
    role: Role,
    index: u32,
    target_tick: u64,
    out: Option<PathBuf>,
    linger: Duration,
    deadline: Instant,
    walk: bool,
    fire: bool,
}

/// The checksum log on disk, written as it goes so a peer that dies leaves what it had.
#[derive(Resource)]
struct Out {
    file: File,
    written_through: Option<u64>,
}

/// Where the peer is in its life, for the exit code.
#[derive(Resource, Default)]
struct Progress {
    session_started: bool,
    reached_target_at: Option<Instant>,
    left: bool,
    last_refresh: Option<Instant>,
    join_sent: bool,
}

fn main() {
    let config = match parse_args() {
        Ok(config) => config,
        Err(message) => {
            eprintln!("netpeer: {message}");
            std::process::exit(2);
        }
    };
    let server_url =
        std::env::var("SIGNALLING_SERVER_URL").unwrap_or_else(|_| "ws://localhost:9090/ws".into());

    let mut app = App::new();
    app.add_plugins((MinimalPlugins, bevy::log::LogPlugin::default()))
        // One tick per `update()`; the runner paces the updates to a tick's worth of wall clock.
        .insert_resource(TimeUpdateStrategy::ManualDuration(TICK))
        .add_plugins(TickedPlugin {
            source: TickSource::Hz(64.0),
            ..default()
        })
        .add_plugins(EnsemblePlugin)
        .add_plugins(BevyEnsembleWebrtcPlugin {
            server_url,
            display_name: match config.role {
                Role::Host => "host".into(),
                Role::Client => format!("client-{}", config.index),
            },
            // Peers on one machine need no STUN and would wait on it anyway.
            ice_servers: bevy_ensemble_webrtc::IceServers::none(),
            ..default()
        })
        .add_plugins((
            TickedServerPlugin::<Input>::new(),
            TickedClientPlugin::<Input>::new(),
            TickedNetworkingEnsemblePlugin::<Input>::new(),
            TickedEnsembleSessionPlugin::default(),
        ))
        .add_plugins(TickedInputPlugin::<Input>::new(scripted_input))
        .insert_resource(config.clone())
        .init_resource::<Progress>()
        .add_systems(Startup, host_if_host)
        .add_systems(
            Update,
            (
                join_when_listed,
                seat_participants,
                watch_session,
                note_leaving,
                pulse,
            ),
        )
        .add_systems(TickedLoop, write_checksums.in_set(TickedSystems::PostTick));
    minimal::install_with_checksums(&mut app);
    if let Some(path) = &config.out {
        let file = File::create(path).unwrap_or_else(|error| {
            eprintln!("netpeer: cannot create {}: {error}", path.display());
            std::process::exit(2);
        });
        app.insert_resource(Out {
            file,
            written_through: None,
        });
    }
    app.finish();
    app.cleanup();

    let mut next = Instant::now();
    loop {
        app.update();
        let progress = app.world().resource::<Progress>();
        let now = Instant::now();
        if progress.left {
            eprintln!("netpeer: the session ended before the target tick");
            std::process::exit(1);
        }
        if let Some(reached) = progress.reached_target_at
            && now >= reached + config.linger
        {
            info!(
                "netpeer: done at tick {}",
                app.world().resource::<CurrentTick>().0
            );
            std::process::exit(0);
        }
        if now >= config.deadline {
            if progress.session_started {
                eprintln!(
                    "netpeer: timed out at tick {} before reaching {}",
                    app.world().resource::<CurrentTick>().0,
                    config.target_tick
                );
                std::process::exit(1);
            }
            eprintln!("netpeer: INCONCLUSIVE — the transport never connected");
            std::process::exit(3);
        }
        next += TICK;
        if let Some(wait) = next.checked_duration_since(now) {
            std::thread::sleep(wait);
        } else {
            next = now;
        }
    }
}

fn parse_args() -> Result<PeerConfig, String> {
    let mut role = None;
    let mut index = 0;
    let mut target_tick = 400;
    let mut out = None;
    let mut linger = Duration::from_millis(2000);
    let mut timeout = Duration::from_secs(60);
    let mut walk = false;
    let mut fire = false;
    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        let mut value = |name: &str| args.next().ok_or_else(|| format!("{name} needs a value"));
        match flag.as_str() {
            "--role" => {
                role = Some(match value("--role")?.as_str() {
                    "host" => Role::Host,
                    "client" => Role::Client,
                    other => return Err(format!("unknown role `{other}`")),
                })
            }
            "--index" => {
                index = value("--index")?
                    .parse()
                    .map_err(|e| format!("--index: {e}"))?
            }
            "--ticks" => {
                target_tick = value("--ticks")?
                    .parse()
                    .map_err(|e| format!("--ticks: {e}"))?
            }
            "--out" => out = Some(PathBuf::from(value("--out")?)),
            "--linger" => {
                linger = Duration::from_millis(
                    value("--linger")?
                        .parse()
                        .map_err(|e| format!("--linger: {e}"))?,
                )
            }
            "--timeout" => {
                timeout = Duration::from_secs(
                    value("--timeout")?
                        .parse()
                        .map_err(|e| format!("--timeout: {e}"))?,
                )
            }
            "--walk" => walk = true,
            "--fire" => fire = true,
            other => return Err(format!("unknown flag `{other}`")),
        }
    }
    Ok(PeerConfig {
        role: role.ok_or("--role host|client is required")?,
        index,
        target_tick,
        out,
        linger,
        deadline: Instant::now() + timeout,
        walk,
        fire,
    })
}

fn host_if_host(config: Res<PeerConfig>, mut start: MessageWriter<StartHosting>) {
    if config.role == Role::Host {
        start.write(StartHosting);
    }
}

/// A client refreshes the lobby list every second until it sees one, then joins it.
fn join_when_listed(
    config: Res<PeerConfig>,
    mut progress: ResMut<Progress>,
    lobbies: Query<(), Or<(With<Lobby>, With<PendingLobby>)>>,
    listed: Option<Res<PublicLobbies>>,
    mut refresh: MessageWriter<RefreshLobbyList>,
    mut join: MessageWriter<JoinWebrtcLobby>,
) {
    if config.role != Role::Client || !lobbies.is_empty() {
        return;
    }
    if let Some(first) = listed.as_ref().and_then(|list| list.0.first()) {
        // One join per listing: the lobby entity arrives a frame later, and a second request
        // in between is refused with a warning.
        if !progress.join_sent {
            join.write(JoinWebrtcLobby(first.lobby_id));
            progress.join_sent = true;
        }
        return;
    }
    let now = Instant::now();
    if progress
        .last_refresh
        .is_none_or(|last| now - last >= Duration::from_secs(1))
    {
        refresh.write(RefreshLobbyList);
        progress.last_refresh = Some(now);
    }
}

/// The host gives every participant a body once it knows their spawner slot — and once it
/// has adopted the host role, so the ids it mints are the authority's.
fn seat_participants(
    mut commands: Commands,
    role: Option<Res<bevy_ticked_networking::server::LocalServerPlayer>>,
    host_lobbies: Query<Entity, (With<Lobby>, With<Host>)>,
    participants: Query<(&LobbyParticipant, &LobbyParticipantOf)>,
    bodies: Query<&Owner, With<EntityKind>>,
    mut allocator: ResMut<TrackedIdAllocator>,
    slots: Option<Res<SpawnerSlots>>,
    local: Option<Res<LocalMultiplayerPlayerId>>,
) {
    if role.is_none() {
        return;
    }
    let Some(lobby) = host_lobbies.iter().next() else {
        return;
    };
    for (participant, of) in &participants {
        if of.0 != lobby
            || bodies
                .iter()
                .any(|owner| owner.0 == participant.player_uuid)
        {
            continue;
        }
        let slot = if local
            .as_ref()
            .is_some_and(|me| me.0 == participant.player_uuid)
        {
            0
        } else {
            match slots
                .as_ref()
                .and_then(|slots| slots.slot_of(participant.player_uuid))
            {
                Some(slot) => slot,
                None => continue,
            }
        };
        let id = allocator.next_authority();
        commands.spawn((
            Pos(0),
            Vel(0),
            EntityKind::PLAYER,
            Owner(participant.player_uuid),
            PlayerSlot(slot),
            id,
        ));
        info!("seated {} in slot {slot}", participant.player_uuid);
    }
}

/// The script: a direction that flips every 64 ticks, a pellet every 32.
fn scripted_input(
    config: Res<PeerConfig>,
    tick: Res<CurrentTick>,
    local: Res<LocalPlayer>,
) -> Option<Input> {
    if local.0 == 0 {
        return None;
    }
    let next = tick.0 + 1;
    let mut input = Input::NONE;
    if config.walk {
        input = if next.is_multiple_of(128) || (next / 64).is_multiple_of(2) {
            Input::RIGHT
        } else {
            Input::LEFT
        };
    }
    if config.fire && next.is_multiple_of(32) {
        input.fire = true;
    }
    Some(input)
}

fn watch_session(
    config: Res<PeerConfig>,
    tick: Res<CurrentTick>,
    mut progress: ResMut<Progress>,
    verified_clients: Query<(), (With<LobbyClient>, With<TickedPeerVerified>)>,
    client: Option<Res<LocalClientPlayer>>,
    replays: Option<Res<ReplayStats>>,
) {
    if !progress.session_started {
        let started = match config.role {
            Role::Host => !verified_clients.is_empty(),
            Role::Client => client.is_some() && replays.is_some_and(|r| r.snapshots_applied > 0),
        };
        if started {
            progress.session_started = true;
            info!("LOG_SESSION_START tick {}", tick.0);
        }
    }
    if progress.session_started
        && progress.reached_target_at.is_none()
        && tick.0 >= config.target_tick
    {
        progress.reached_target_at = Some(Instant::now());
        info!("reached tick {}", tick.0);
    }
}

/// Once a second: what the session looks like from here.
fn pulse(
    mut last: Local<Option<Instant>>,
    held: Option<Res<bevy_ensemble::HeldUntilVerified>>,
    host_uuid: Option<Res<bevy_ensemble::HostUuid>>,
    lobbies: Query<(Has<Lobby>, Has<PendingLobby>, Has<Host>)>,
    clients: Query<(
        &bevy_ensemble::LobbyClientPlayerUuid,
        Has<LobbyClient>,
        Has<TickedPeerVerified>,
    )>,
    metrics: Option<Res<bevy_ensemble::NetMetrics>>,
    tick: Res<CurrentTick>,
) {
    let now = Instant::now();
    if last.is_some_and(|at| now - at < Duration::from_secs(1)) {
        return;
    }
    *last = Some(now);
    let mut peers: Vec<u128> = clients.iter().map(|(uuid, _, _)| uuid.0).collect();
    if let Some(host) = &host_uuid {
        peers.push(host.0);
    }
    let held: Vec<usize> = peers
        .iter()
        .map(|p| held.as_ref().map_or(0, |h| h.held_for(*p)))
        .collect();
    let lobbies: Vec<_> = lobbies.iter().collect();
    let clients: Vec<_> = clients.iter().map(|(_, a, b)| (a, b)).collect();
    let metrics = metrics.map(|m| {
        format!(
            "rx {} msgs/{} pkts, tx {} msgs/{} pkts",
            m.rx_messages, m.rx_packets, m.tx_messages, m.tx_packets
        )
    });
    info!(
        "pulse: tick {} held {:?} lobbies(lobby,pending,host) {:?} clients(promoted,verified) {:?} {:?}",
        tick.0, held, lobbies, clients, metrics
    );
}

fn note_leaving(mut left: MessageReader<LobbyLeft>, mut progress: ResMut<Progress>) {
    for message in left.read() {
        warn!("left the lobby: {:?}", message.reason);
        progress.left = true;
    }
}

/// One line per confirmed tick this file does not have yet.
///
/// Nothing before the session started: a client's solo ticks before its join share tick
/// numbers with the host's and describe a different world. The host hashes its live world
/// (the checksum log). A client hashes the authoritative records it was sent for the tick:
/// its own predicted body agrees with them once confirmed, and a remote body it draws
/// interpolated is behind them by design. What is compared is what crossed the wire.
fn write_checksums(
    out: Option<ResMut<Out>>,
    progress: Res<Progress>,
    config: Res<PeerConfig>,
    mut log: ResMut<ChecksumLog<MinimalHash>>,
    registry: Res<bevy_ticked::registry::TickedComponentRegistry>,
    history: Option<Res<bevy_ticked_networking::replication::AuthoritativeHistory>>,
    tick: Res<CurrentTick>,
    applied: Option<Res<bevy_ticked_networking::client::AppliedSnapshotTick>>,
    mut started: Local<bool>,
) {
    let Some(mut out) = out else { return };
    if !progress.session_started {
        return;
    }
    if !*started {
        *started = true;
        log.clear();
        out.written_through = None;
        return;
    }
    let confirmed_through = match config.role {
        Role::Host => tick.0,
        Role::Client => match applied.and_then(|applied| applied.0) {
            Some(applied) => applied.min(tick.0),
            None => return,
        },
    };
    let from = out.written_through.map_or(0, |t| t + 1);
    for t in from..=confirmed_through {
        let hash = match config.role {
            Role::Host => log.at(t),
            Role::Client => history
                .as_ref()
                .and_then(|history| hash_of_records(&registry, history, t)),
        };
        if let Some(hash) = hash {
            let _ = writeln!(
                out.file,
                "{t} {:016x} {} {} {}",
                bevy_ticked::checksum::WorldHash::value(&hash),
                hash.bodies,
                hash.positions,
                hash.velocities
            );
        }
    }
    let _ = out.file.flush();
    if confirmed_through >= from {
        out.written_through = Some(confirmed_through);
    }
}

/// The fixture's hash over the authoritative records at `tick`, decoded the way a snapshot is.
fn hash_of_records(
    registry: &bevy_ticked::registry::TickedComponentRegistry,
    history: &bevy_ticked_networking::replication::AuthoritativeHistory,
    tick: u64,
) -> Option<MinimalHash> {
    if !history.has_tick(tick) {
        return None;
    }
    let pos_index = registry.wire_index_of::<Pos>()?;
    let vel_index = registry.wire_index_of::<Vel>()?;
    let mut rows = Vec::new();
    for id in history.ids_at(tick) {
        let record = history.at(tick, id)?;
        let parts = registry.split_record(&record.present, &record.bytes)?;
        let mut pos = None;
        let mut vel = None;
        for (index, range) in parts {
            if index == pos_index {
                pos = postcard::from_bytes::<Pos>(&record.bytes[range]).ok();
            } else if index == vel_index {
                vel = postcard::from_bytes::<Vel>(&record.bytes[range]).ok();
            }
        }
        if let (Some(pos), Some(vel)) = (pos, vel) {
            rows.push((id, pos.0, vel.0));
        }
    }
    Some(MinimalHash::from_rows(rows))
}
