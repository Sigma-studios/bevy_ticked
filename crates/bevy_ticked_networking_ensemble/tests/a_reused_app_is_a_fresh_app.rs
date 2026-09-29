//! A reused app is a fresh app: whatever sessions a peer has been through, the next one starts
//! exactly as it would on a peer that was never in one.
//!
//! Driven by the session registry, not by a list: every resource any crate registered with
//! `init_session_resource` and friends is read on a peer that has been through a session and on
//! one that has not, and they must print the same. A resource added to the stack tomorrow is in
//! this test the day it is registered, which is the property the hand-kept door lists never had —
//! the reason the registry exists.
//!
//! Each scenario ends with a door, and the two apps are compared the frame that door ran: a join
//! against a first join, a hosting against a first hosting, a host change against a first join
//! of the same uuid. And after every leave, the reused peer is compared with a peer that has idled
//! in the menu for the same frames.

use std::collections::BTreeMap;

use bevy::prelude::*;
use bevy_ensemble::{Host, Lobby, PendingLobby};
use bevy_ticked::prelude::*;
use bevy_ticked::session::SessionResources;
use bevy_ticked_networking::client::ClientTickBuffer;
use bevy_ticked_networking::input::InputQueue;
use bevy_ticked_networking::server::{InputMargins, MeasuredMargin, NewestInputTick, PassesHeld};
use bevy_ticked_networking::session::TickedSession;
use bevy_ticked_networking_ensemble::{EndSession, SpawnerSlots, StartSolo};
use bevy_ticked_testing::fixtures::minimal::{self, Input, seat_everyone};
use bevy_ticked_testing::prelude::*;

const UUID: u128 = 7;

type Census = BTreeMap<&'static str, Option<String>>;

/// Every registered session resource, printed, plus the clock and the world.
fn census(world: &mut World) -> Census {
    let mut out: Census = world
        .resource::<SessionResources>()
        .clone()
        .fingerprints(world)
        .into_iter()
        .collect();
    out.insert(
        "CurrentTick",
        Some(format!("{:?}", world.resource::<CurrentTick>())),
    );
    out.insert(
        "TickedSession",
        Some(format!(
            "{:?}",
            world.resource::<State<TickedSession>>().get()
        )),
    );
    let tracked = world
        .query_filtered::<(), (
            With<TickTrackedEntity>,
            bevy::ecs::query::Allow<bevy::ecs::entity_disabling::Disabled>,
        )>()
        .iter(world)
        .count();
    out.insert("tracked entities", Some(tracked.to_string()));
    out
}

fn assert_same(what: &str, reused: &Census, fresh: &Census) {
    let differences: Vec<String> = fresh
        .keys()
        .chain(reused.keys())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .filter(|name| reused.get(*name) != fresh.get(*name))
        .map(|name| {
            format!(
                "{name}:\n      reused {:?}\n      fresh  {:?}",
                reused.get(name).cloned().flatten(),
                fresh.get(name).cloned().flatten()
            )
        })
        .collect();
    assert!(
        differences.is_empty(),
        "{what}: session state outlived the session\n  {}",
        differences.join("\n  ")
    );
}

fn peer() -> App {
    client_server_peer::<Input>(UUID, minimal::install)
}

/// What a backend does when a hosted lobby is created, or a joined one promoted: the lobby entity,
/// which the bridge adopts a role from.
fn open_lobby(app: &mut App, host: bool) {
    if host {
        app.world_mut().spawn((Lobby, Host));
    } else {
        app.world_mut().spawn(Lobby);
    }
}

fn step(app: &mut App, frames: usize) {
    for _ in 0..frames {
        app.update();
    }
}

/// The lobby appears, the bridge adopts in `Update`, and the door runs in the next frame's
/// `StateTransition`: two frames.
fn enter(app: &mut App, host: bool) {
    open_lobby(app, host);
    step(app, 2);
}

fn leave(app: &mut App) {
    app.world_mut().write_message(EndSession);
    step(app, 2);
    assert_eq!(
        *app.world().resource::<State<TickedSession>>().get(),
        TickedSession::Offline
    );
}

/// Make a session's state as untidy as a long one leaves it: a lead that was being steered, a
/// replay budget, inputs and margins from clients, a slot handed out, a held broadcast.
fn play_a_while(app: &mut App) {
    step(app, 20);
    let world = app.world_mut();
    let tick = world.resource::<CurrentTick>().0;
    world
        .resource_mut::<InputQueue<Input>>()
        .insert(tick + 1, 3, Input::RIGHT);
    world
        .resource_mut::<NewestInputTick>()
        .0
        .insert(3, tick + 1);
    world
        .resource_mut::<InputMargins>()
        .0
        .insert(3, MeasuredMargin { ticks: 2, at: tick });
    world.resource_mut::<SpawnerSlots>().assign(3);
    world.resource_mut::<PassesHeld>().0 = 17;
    {
        let mut buffer = world.resource_mut::<ClientTickBuffer>();
        buffer.target_replay_distance = 40;
        buffer.target_margin = 9;
    }
    step(app, 5);
}

/// A peer that idled in the menu for as long as `reused` has lived since its last leave: nothing
/// registered may differ.
fn assert_left_cleanly(what: &str, reused: &mut App, frames_alive: usize) {
    let mut fresh = peer();
    step(&mut fresh, frames_alive);
    assert_same(
        what,
        &census(reused.world_mut()),
        &census(fresh.world_mut()),
    );
}

#[test]
fn host_leave_join_is_a_first_join() {
    let mut reused = peer();
    step(&mut reused, 3);
    enter(&mut reused, true);
    play_a_while(&mut reused);
    leave(&mut reused);
    // The leave frame: tick 0 at the door, and one tick run by the loop after it.
    let mut idle = peer();
    step(&mut idle, 2);
    assert_same(
        "after leaving a hosted session",
        &census(reused.world_mut()),
        &census(idle.world_mut()),
    );

    enter(&mut reused, false);
    let mut fresh = peer();
    step(&mut fresh, 3);
    enter(&mut fresh, false);
    assert_same(
        "joining after hosting",
        &census(reused.world_mut()),
        &census(fresh.world_mut()),
    );
}

#[test]
fn client_leave_client_is_a_first_join() {
    let mut reused = peer();
    enter(&mut reused, false);
    play_a_while(&mut reused);
    leave(&mut reused);
    assert_left_cleanly("after leaving as a client", &mut reused, 2);

    enter(&mut reused, false);
    let mut fresh = peer();
    enter(&mut fresh, false);
    assert_same(
        "joining again",
        &census(reused.world_mut()),
        &census(fresh.world_mut()),
    );
}

#[test]
fn solo_leave_host_is_a_first_hosting() {
    let mut reused = peer();
    reused.world_mut().write_message(StartSolo);
    step(&mut reused, 2);
    assert_eq!(
        *reused.world().resource::<State<TickedSession>>().get(),
        TickedSession::Solo
    );
    play_a_while(&mut reused);
    leave(&mut reused);
    assert_left_cleanly("after leaving a solo session", &mut reused, 2);

    enter(&mut reused, true);
    let mut fresh = peer();
    step(&mut fresh, 2);
    enter(&mut fresh, true);
    assert_same(
        "hosting after playing alone",
        &census(reused.world_mut()),
        &census(fresh.world_mut()),
    );
}

/// A solo world handed to a lobby it hosts keeps its world, and nothing else of the solo session:
/// the host that played alone first is a host that did not, but for the entities standing.
#[test]
fn a_solo_world_handed_to_a_hosted_lobby_keeps_only_the_world() {
    let mut reused = peer();
    reused.world_mut().write_message(StartSolo);
    step(&mut reused, 2);
    play_a_while(&mut reused);
    reused.world_mut().spawn_tracked(minimal::Pos(5));
    // A host adopts as soon as the backend knows its identity, which the harness does from the
    // start: the lobby is still pending, and the solo world becomes the hosted one at once.
    reused.world_mut().spawn((PendingLobby, Host));
    step(&mut reused, 2);
    assert_eq!(
        *reused.world().resource::<State<TickedSession>>().get(),
        TickedSession::Host
    );

    let mut fresh = peer();
    step(&mut fresh, 29);
    fresh.world_mut().spawn_tracked(minimal::Pos(5));
    enter(&mut fresh, true);
    let mut reused_census = census(reused.world_mut());
    let mut fresh_census = census(fresh.world_mut());
    // The allocator is the world's, raised over it either way; compare the rest.
    for census in [&mut reused_census, &mut fresh_census] {
        census.remove("tracked entities");
    }
    assert_same("hosting from a solo world", &reused_census, &fresh_census);
    let mut tracked = reused.world_mut().query::<&TickTrackedEntity>();
    assert_eq!(
        tracked.iter(reused.world()).count(),
        1,
        "the world built alone is the session's world"
    );
}

/// A host change re-enters every survivor's role in one door. The client that stays is, the frame
/// it re-enters, a client that has just joined for the first time.
#[test]
fn a_host_change_is_a_first_join_for_the_client_that_stays() {
    #[derive(Resource, Default)]
    struct ClientEntries(u32);

    let mut net = TickedNetwork::client_server::<Input>(2, |app| {
        minimal::install(app);
        app.init_resource::<ClientEntries>().add_systems(
            OnEnter(TickedSession::Client),
            |mut entries: ResMut<ClientEntries>| entries.0 += 1,
        );
    })
    .with_link(Link::cable())
    .with_seed(11)
    .with_host_migration(HostMigratable {
        successor_within: std::time::Duration::from_secs(4),
        reach_within: std::time::Duration::from_secs(20),
    });
    assert!(net.settle(400), "the session did not settle");
    seat_everyone(&mut net);
    net.run(60);
    let clients = net.clients();
    let (a, b) = (clients[0], clients[1]);
    let entries = |net: &TickedNetwork| net.app(b).world().resource::<ClientEntries>().0;
    let before = entries(&net);

    net.migrate(a);
    let reentered = (0..200).any(|_| {
        net.step();
        entries(&net) > before
    });
    assert!(reentered, "b never re-entered its client role");
    assert_eq!(
        *net.app(b).world().resource::<State<TickedSession>>().get(),
        TickedSession::Client,
        "b is a client of the new host, without a frame out of the role"
    );

    let uuid = net.uuid(b);
    let mut fresh = client_server_peer::<Input>(uuid, minimal::install);
    fresh.world_mut().spawn(Lobby);
    step(&mut fresh, 2);
    let mut reused = census(net.world_mut(b));
    let mut fresh = census(fresh.world_mut());
    // The lobby's own bookkeeping — who has been handed which slot on the *host* — has no
    // counterpart on a peer with no lobby to take it from.
    for census in [&mut reused, &mut fresh] {
        census.remove(std::any::type_name::<SpawnerSlots>());
        // And the handshake's clock does not run while the lobby is still reaching its new host,
        // which a first join's lobby never is.
        census.remove(std::any::type_name::<
            bevy_ticked_networking_ensemble::HandshakeWait,
        >());
    }
    assert_same("a client after a host change", &reused, &fresh);
}
