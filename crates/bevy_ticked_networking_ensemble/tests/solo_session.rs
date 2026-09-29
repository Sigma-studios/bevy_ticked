//! Playing alone as a session, and the leave hook a game registers its own state with.
//!
//! Both games that can play alone wrote the same thing by hand: a `SoloPlayer` resource and a
//! `LocalPlayer` set to a made-up uuid on the way in, a lobby appearing ending it, and a
//! `reset_on_leave` system watching "no lobby and no solo" to put the game's own resources back.
//! These pin the upstream version of each.

use bevy::prelude::*;
use bevy_ensemble::{Host, Lobby, PendingLobby};
use bevy_ticked::prelude::*;
use bevy_ticked_networking::input_plugin::LocalPlayer;
use bevy_ticked_networking::session::{LocalSoloPlayer, TickedSession};
use bevy_ticked_networking_ensemble::{EndSession, StartSolo, in_session};
use bevy_ticked_testing::fixtures::minimal::{self, Input, Pos};
use bevy_ticked_testing::prelude::*;

/// A game's own session state: what it joined by, which it sets *before* the session starts.
#[derive(Resource, Default, Debug, PartialEq)]
struct JoinedCode(Option<String>);

impl SessionReset for JoinedCode {}

#[derive(Resource, Default)]
struct InSession(bool);

fn peer() -> App {
    client_server_peer::<Input>(7, |app| {
        minimal::install(app);
        app.init_session_resource::<JoinedCode>()
            .init_resource::<InSession>()
            .add_systems(
                PostUpdate,
                (|mut flag: ResMut<InSession>| flag.0 = true).run_if(in_session),
            )
            .add_systems(
                PostUpdate,
                (|mut flag: ResMut<InSession>| flag.0 = false).run_if(not(in_session)),
            );
    })
}

fn state(app: &App) -> TickedSession {
    *app.world().resource::<State<TickedSession>>().get()
}

fn step(app: &mut App, frames: usize) {
    for _ in 0..frames {
        app.update();
    }
}

fn tracked(app: &mut App) -> usize {
    app.world_mut()
        .query::<&TickTrackedEntity>()
        .iter(app.world())
        .count()
}

#[test]
fn start_solo_enters_a_solo_session_under_the_solo_uuid() {
    let mut app = peer();
    app.update();
    assert!(!app.world().resource::<InSession>().0);
    app.world_mut().write_message(StartSolo);
    step(&mut app, 2);
    assert_eq!(state(&app), TickedSession::Solo);
    assert_eq!(*app.world().resource::<LocalPlayer>(), LocalPlayer(1));
    assert!(app.world().resource::<InSession>().0, "in_session follows");
}

#[test]
fn ending_a_solo_session_is_a_leave() {
    let mut app = peer();
    app.world_mut().write_message(StartSolo);
    step(&mut app, 2);
    app.world_mut().spawn_tracked(Pos(3));
    app.insert_resource(JoinedCode(Some("ABCD".into())));
    step(&mut app, 30);
    assert!(app.world().resource::<CurrentTick>().0 > 20);

    app.world_mut().write_message(EndSession);
    step(&mut app, 2);
    assert_eq!(state(&app), TickedSession::Offline);
    assert_eq!(tracked(&mut app), 0, "the solo world goes with the session");
    assert_eq!(*app.world().resource::<LocalPlayer>(), LocalPlayer(0));
    assert_eq!(
        *app.world().resource::<JoinedCode>(),
        JoinedCode(None),
        "and so does what the game registered for it"
    );
    assert!(
        app.world().resource::<CurrentTick>().0 <= 1,
        "the clock starts over: what games reset by hand when leaving practice"
    );
    assert!(!app.world().resource::<InSession>().0);
}

/// Opening a lobby from a solo game keeps the world and everything the game holds for the visit:
/// the lobby is the same visit, and the arena the players stand in must not be rebuilt under them.
#[test]
fn hosting_from_solo_keeps_the_world_and_the_games_session_state() {
    let mut app = peer();
    app.world_mut().write_message(StartSolo);
    step(&mut app, 2);
    app.world_mut().spawn_tracked(Pos(3));
    app.insert_resource(JoinedCode(Some("ABCD".into())));
    step(&mut app, 10);

    app.world_mut().spawn((Lobby, Host));
    step(&mut app, 2);
    assert_eq!(state(&app), TickedSession::Host);
    assert_eq!(
        tracked(&mut app),
        1,
        "the world built alone is the session's"
    );
    assert!(!app.world().contains_resource::<LocalSoloPlayer>());
    assert_eq!(*app.world().resource::<LocalPlayer>(), LocalPlayer(7));
    assert_eq!(
        *app.world().resource::<JoinedCode>(),
        JoinedCode(Some("ABCD".into()))
    );
}

/// Joining from solo: the world is handed over while the lobby forms — still this peer's, still
/// ticking — and replaced by the host's the moment the client role is taken.
#[test]
fn joining_from_solo_replaces_the_world_at_the_join() {
    let mut app = peer();
    app.world_mut().write_message(StartSolo);
    step(&mut app, 2);
    app.world_mut().spawn_tracked(Pos(3));
    step(&mut app, 5);

    let lobby = app.world_mut().spawn(PendingLobby).id();
    step(&mut app, 3);
    assert_eq!(
        state(&app),
        TickedSession::Solo,
        "a lobby forming does not end the session under the player"
    );
    assert_eq!(tracked(&mut app), 1);

    app.world_mut()
        .entity_mut(lobby)
        .remove::<PendingLobby>()
        .insert(Lobby);
    step(&mut app, 2);
    assert_eq!(state(&app), TickedSession::Client);
    assert_eq!(tracked(&mut app), 0, "a client's world is its host's");
    assert!(
        app.world()
            .resource::<TickHolds>()
            .holds(TickHoldReason::AwaitingSync)
    );
}

/// A lobby that goes before it gives a role — a host request the server refused — hands the solo
/// session its world back: a network error does not throw away the player's practice.
#[test]
fn a_lobby_that_never_gives_a_role_hands_the_solo_world_back() {
    let mut app = peer();
    app.world_mut().write_message(StartSolo);
    step(&mut app, 2);
    app.world_mut().spawn_tracked(Pos(3));
    let lobby = app.world_mut().spawn(PendingLobby).id();
    step(&mut app, 3);
    app.world_mut().despawn(lobby);
    step(&mut app, 3);
    assert_eq!(state(&app), TickedSession::Solo);
    assert_eq!(tracked(&mut app), 1, "the practice world is still standing");
    assert_eq!(*app.world().resource::<LocalPlayer>(), LocalPlayer(1));

    // And the next lobby is handed it again, as the first was.
    app.world_mut().spawn((Lobby, Host));
    step(&mut app, 2);
    assert_eq!(state(&app), TickedSession::Host);
    assert_eq!(tracked(&mut app), 1);
}

/// A join the server refused never gives a role, and still ends in a leave: what the game set on
/// the way in — the code it joined by — goes back, as the games' own `reset_on_leave` did.
#[test]
fn a_refused_join_still_ends_in_a_leave() {
    let mut app = peer();
    app.update();
    app.insert_resource(JoinedCode(Some("WXYZ".into())));
    let lobby = app.world_mut().spawn(PendingLobby).id();
    step(&mut app, 3);
    assert!(app.world().resource::<InSession>().0);
    assert_eq!(state(&app), TickedSession::Offline);

    app.world_mut().despawn(lobby);
    step(&mut app, 3);
    assert_eq!(*app.world().resource::<JoinedCode>(), JoinedCode(None));
    assert!(!app.world().resource::<InSession>().0);
}

/// A game's session state survives the join itself — it was set before the role existed — and
/// goes on the leave.
#[test]
fn a_games_session_state_lasts_from_before_the_join_to_the_leave() {
    let mut app = peer();
    app.update();
    app.insert_resource(JoinedCode(Some("WXYZ".into())));
    app.world_mut().spawn(Lobby);
    step(&mut app, 3);
    assert_eq!(state(&app), TickedSession::Client);
    assert_eq!(
        *app.world().resource::<JoinedCode>(),
        JoinedCode(Some("WXYZ".into()))
    );
    app.world_mut().write_message(EndSession);
    step(&mut app, 2);
    assert_eq!(state(&app), TickedSession::Offline);
    assert_eq!(*app.world().resource::<JoinedCode>(), JoinedCode(None));
}

/// What arrives in the frame between the client role being taken and its join door must survive a
/// join from solo: the host's first snapshot, its welcome (the spawner slot), and the link's round
/// trip for sizing the prediction buffer. Each was lost on this path once — the door out of `Solo`
/// cleared the snapshot and the slot, and the snapshot then released the hold the seed waited on.
#[test]
fn joining_from_solo_keeps_what_arrived_before_the_join_door() {
    use bevy_ensemble::{PeerRtt, ReceivedEnsembleMessage};
    use bevy_ticked_networking::client::{AppliedSnapshotTick, ClientTickBuffer};
    use bevy_ticked_networking::snapshot::{FullBody, SnapshotBody, SnapshotPacket, encode_packet};
    use bevy_ticked_networking_ensemble::{
        EnsembleSnapshotMessage, LocalSpawnerSlot, RegistryVerified, TickBufferSeeded,
        TickedSessionWelcome,
    };

    fn snapshot(tick: u64) -> ReceivedEnsembleMessage<EnsembleSnapshotMessage> {
        ReceivedEnsembleMessage {
            sender: Some(HOST_UUID),
            message: EnsembleSnapshotMessage {
                bytes: encode_packet(&SnapshotPacket {
                    seq: tick as u32,
                    tick,
                    your_margin: 0,
                    body: SnapshotBody::Full(FullBody::default()),
                }),
            },
            received_at: bevy_ensemble::Instant::now(),
        }
    }

    let mut app = peer();
    app.world_mut().write_message(StartSolo);
    step(&mut app, 10);
    assert_eq!(state(&app), TickedSession::Solo);

    // The host's registries matched and the lobby is promoted, with a ping already back.
    app.insert_resource(RegistryVerified);
    app.world_mut().spawn((Lobby, PeerRtt(0.2)));
    // The frame the role is taken: the host's welcome arrives with it.
    app.world_mut().write_message(ReceivedEnsembleMessage {
        sender: Some(HOST_UUID),
        message: TickedSessionWelcome {
            slot: 5,
            server_tick: 1000,
            send_every: 1,
        },
        received_at: bevy_ensemble::Instant::now(),
    });
    app.update();
    assert_eq!(state(&app), TickedSession::Solo, "the door runs next frame");

    // The door's frame: a snapshot is forwarded in `PreUpdate`, before the door.
    app.world_mut().write_message(snapshot(1000));
    app.update();
    assert_eq!(state(&app), TickedSession::Client);
    assert_eq!(
        app.world().resource::<AppliedSnapshotTick>().0,
        Some(1000),
        "the snapshot that arrived before the door is applied after it, the same frame"
    );
    assert_eq!(
        app.world()
            .get_resource::<LocalSpawnerSlot>()
            .map(|slot| slot.0.0),
        Some(5),
        "the welcome's slot survives the door out of solo"
    );
    assert!(
        app.world().resource::<TickBufferSeeded>().0,
        "the buffer was sized from the link at the door, before the snapshot closed the window"
    );
    assert!(
        app.world()
            .resource::<ClientTickBuffer>()
            .target_replay_distance
            > 6,
        "a 200 ms round trip is more than the default guess"
    );
}
