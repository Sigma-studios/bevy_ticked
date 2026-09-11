//! Snapshot wire v2 over the harness: the registry handshake gates the world, a mismatch is
//! named, packets are addressed to one client each, and two walking players cost what the
//! audit said they should.
//!
//! Every test here is about the join and the wire rather than the simulation, so the fixture is
//! the harness's integer game: a body per player, `Pos`/`Vel`/`EntityKind`/`Owner` on the wire.
//! The audit measured 1340 bytes a tick for two walking players under the old type-major,
//! `HashMap`-encoded shape; the last test prints what the entity-major shape costs.

use std::time::Duration;

use bevy::prelude::*;
use bevy_ticked::tick::CurrentTick;
use bevy_ticked_networking::client::LocalClientPlayer;
use bevy_ticked_networking::input::InputQueue;
use bevy_ticked_networking::networked_registry::NetworkedTickedAppExt;
use bevy_ticked_networking::server::{InputMargins, SnapshotRecipientList};
use bevy_ticked_networking::snapshot::{FullBody, SnapshotBody, SnapshotPacket, encode_packet};
use bevy_ticked_networking_ensemble::{
    EnsembleSnapshotMessage, HandshakeTimedOut, HandshakeTimeout, LocalSpawnerSlot,
    RegistryMismatch, RegistryVerified, SpawnerSlots,
};
use bevy_ticked_testing::fixtures::minimal::{self, EntityKind, Input, Vel, seat_everyone};
use bevy_ticked_testing::log::{errors_since, mark};
use bevy_ticked_testing::prelude::*;

const SETTLE: usize = 400;

/// A peer from a commit that never registered `Pos`: the other names, systems and all.
fn without_pos(app: &mut App) {
    use bevy_ticked_testing::fixtures::minimal::{Fuse, PlayerSlot};
    minimal::install_systems(app);
    app.register_networked_ticked_component::<Vel>("Vel")
        .register_networked_ticked_component::<EntityKind>("EntityKind")
        .register_networked_ticked_component::<PlayerSlot>("PlayerSlot")
        .register_networked_ticked_component::<Fuse>("Fuse");
}

fn host_margins(net: &TickedNetwork) -> Vec<(u128, i64)> {
    let mut margins: Vec<(u128, i64)> = net
        .app(net.host())
        .world()
        .resource::<InputMargins>()
        .0
        .iter()
        .map(|(uuid, margin)| (*uuid, *margin))
        .collect();
    margins.sort_unstable();
    margins
}

fn host_margin_for(net: &TickedNetwork, uuid: u128) -> Option<i64> {
    net.app(net.host())
        .world()
        .resource::<InputMargins>()
        .0
        .get(&uuid)
        .copied()
}

fn recipients(net: &TickedNetwork) -> Vec<u128> {
    net.app(net.host())
        .world()
        .resource::<SnapshotRecipientList>()
        .0
        .clone()
}

fn slot_on(net: &TickedNetwork, peer: PeerId) -> Option<u8> {
    net.app(peer)
        .world()
        .get_resource::<LocalSpawnerSlot>()
        .map(|slot| slot.0.0)
}

// ── The gate ─────────────────────────────────────────────────────────────────

/// Until the client has compared the host's registries with its own, nothing it receives is
/// applied — not even a well-formed snapshot that arrives first, which is what the crafted
/// packet is. The drop is counted where a test can read it.
#[test]
fn no_snapshot_is_applied_before_the_registry_handshake_matches() {
    let mut net = TickedNetwork::client_server::<Input>(1, minimal::install)
        .with_link(Link::four_g())
        .with_seed(5);
    let (host, client) = (net.host(), net.client());
    let host_uuid = net.uuid(host);

    // A snapshot for tick 1 of an empty world, handed to the client before any handshake has
    // had a chance to cross. Applying it would set the client's applied tick.
    let early = encode_as(
        net.app(host),
        &EnsembleSnapshotMessage {
            bytes: encode_packet(&SnapshotPacket {
                seq: 1,
                tick: 1,
                your_margin: 0,
                body: SnapshotBody::Full(FullBody::default()),
            }),
        },
    );
    deliver_raw(&mut net, client, host_uuid, early);

    let mut frames_unverified = 0;
    for _ in 0..SETTLE {
        net.step();
        if net
            .app(client)
            .world()
            .contains_resource::<RegistryVerified>()
        {
            break;
        }
        frames_unverified += 1;
        assert_eq!(
            applied_tick(net.app(client)),
            None,
            "a snapshot was applied before the registries were compared"
        );
        assert!(
            tracked_ids(net.app_mut(client)).is_empty(),
            "an entity appeared before the registries were compared"
        );
    }
    assert!(
        net.app(client)
            .world()
            .contains_resource::<RegistryVerified>(),
        "the handshake never matched in {SETTLE} frames"
    );
    assert!(
        frames_unverified > 0,
        "the client was verified on its first frame, so nothing above was checked"
    );

    assert!(
        net.settle(SETTLE),
        "the session did not settle after the handshake"
    );
    let stats = replays(net.app(client));
    assert!(
        stats.dropped_before_handshake >= 1,
        "the snapshot delivered before the handshake was not counted as dropped: {stats:?}"
    );
    assert!(
        stats.snapshots_applied > 0,
        "and snapshots do flow once verified"
    );
}

/// A client built from a commit with one registration fewer joins, is told the difference, and
/// never holds a single tracked entity: the host never sends it a snapshot and it would not
/// apply one.
#[test]
fn a_mismatched_client_never_spawns_an_entity() {
    let mut net = TickedNetwork::client_server::<Input>(0, minimal::install)
        .with_link(Link::cable())
        .with_seed(2);
    let bad = net.add_client_built(without_pos);
    let host = net.host();
    net.trace_packets();
    assert!(net.run_until(60, |net| role(net.app(host)) == Role::Host));
    seat_everyone(&mut net);

    for frame in 0..300 {
        net.step();
        assert!(
            tracked_ids(net.app_mut(bad)).is_empty(),
            "frame {frame}: a tracked entity appeared on a client whose registries do not match"
        );
    }

    assert!(
        net.app(bad).world().contains_resource::<RegistryMismatch>(),
        "the client never found out its registries differ"
    );
    assert_eq!(
        role(net.app(bad)),
        Role::Solo,
        "the client role must be dropped, and not taken back"
    );
    assert!(
        !paused(net.app(bad)),
        "a refused client must not sit holding AwaitingSync for a world that is not coming"
    );
    assert!(
        net.snapshot_packets(host, bad).is_empty(),
        "the host sent a snapshot to a client it never verified"
    );
    assert!(
        !tracked_ids(net.app_mut(host)).is_empty(),
        "the host's own world is untouched by one client's mismatch"
    );
    assert_eq!(role(net.app(host)), Role::Host);
}

#[test]
fn a_mismatched_client_is_told_which_registration_differs() {
    let mut net = TickedNetwork::client_server::<Input>(0, minimal::install)
        .with_link(Link::cable())
        .with_seed(4);
    let log = mark();
    let bad = net.add_client_built(without_pos);
    assert!(
        net.run_until(120, |net| net
            .app(bad)
            .world()
            .contains_resource::<RegistryMismatch>()),
        "the mismatch was never found"
    );

    let mismatch = net.app(bad).world().resource::<RegistryMismatch>().clone();
    assert_eq!(
        mismatch.difference,
        "the peer registers component \"Pos\" which this build does not"
    );
    assert_eq!(mismatch.peer, net.uuid(net.host()));
    assert_eq!(
        mismatch.theirs.component_names,
        [
            "EntityKind",
            "Fuse",
            "PlayerSlot",
            "Pos",
            "Vel",
            "bevy_ticked::Owner"
        ],
        "the host's sorted names travel with the handshake"
    );
    assert_eq!(
        mismatch.ours.component_names,
        [
            "EntityKind",
            "Fuse",
            "PlayerSlot",
            "Vel",
            "bevy_ticked::Owner"
        ]
    );

    // The host's side of the same story names the same registration.
    net.run(2);
    let errors = errors_since(&log);
    assert!(
        errors
            .iter()
            .any(|line| line.contains("refusing client") && line.contains("\"Pos\"")),
        "the host did not log which registration differs: {errors:?}"
    );
}

/// A client whose announcement never reaches the host is never told the host's registries;
/// after the timeout it stops waiting, says so, and is not left paused.
#[test]
fn a_client_that_never_completes_the_handshake_is_refused_after_the_timeout() {
    let mut net = TickedNetwork::client_server::<Input>(0, minimal::install)
        .with_link(Link::cable())
        .with_seed(3);
    let client = net.add_client_with(|app| {
        app.insert_resource(HandshakeTimeout(Duration::from_millis(500)));
    });
    // Heard by nobody: the client's protocol handshake never reaches the host, so the host
    // never marks it verified and never announces its registries to it.
    net.half_open(client);

    net.run(16);
    assert_eq!(
        role(net.app(client)),
        Role::Client,
        "a quarter of a second in, the client is still waiting"
    );
    assert!(
        !net.app(client)
            .world()
            .contains_resource::<HandshakeTimedOut>(),
        "the timeout fired early"
    );

    net.run(48);
    let app = net.app(client);
    let timed_out = app
        .world()
        .get_resource::<HandshakeTimedOut>()
        .expect("a second in, the client has given up");
    assert!(timed_out.waited >= Duration::from_millis(500));
    assert_eq!(
        role(app),
        Role::Solo,
        "the role is dropped and not taken back"
    );
    assert!(
        !paused(app),
        "and the clock is released, not left on AwaitingSync"
    );
    assert!(!app.world().contains_resource::<RegistryVerified>());
    assert_eq!(applied_tick(app), None);
}

// ── Addressed packets ────────────────────────────────────────────────────────

/// Two clients on different links have different input margins, and each packet carries the
/// margin of the client it goes to: the host's own figure for that uuid, and nobody else's.
#[test]
fn each_client_sees_only_its_own_margin() {
    let mut net = TickedNetwork::client_server::<Input>(2, minimal::install).with_seed(9);
    let host = net.host();
    let clients = net.clients();
    let (near, far) = (clients[0], clients[1]);
    net.net
        .set_link_pair(host, near, Link::cable(), Link::cable());
    net.net
        .set_link_pair(host, far, Link::satellite(), Link::satellite());
    assert!(net.settle(SETTLE * 2), "the session did not settle");
    seat_everyone(&mut net);
    let (near_uuid, far_uuid) = (net.uuid(near), net.uuid(far));

    net.trace_packets();
    let script = ActionScript::new()
        .hold(0, 200, near_uuid, Input::RIGHT)
        .hold(0, 200, far_uuid, Input::LEFT);
    net.run_input_script(&script, 0);

    let to_near = net.decode_snapshots(host, near);
    let to_far = net.decode_snapshots(host, far);
    assert!(
        to_near.len() > 100 && to_far.len() > 100,
        "few snapshots were traced"
    );

    // What the host holds now is what the last packet of the frame carried, because margins
    // move in PreUpdate and the snapshot is built after the tick.
    let near_margin = host_margin_for(&net, near_uuid).expect("the host has heard from near");
    let far_margin = host_margin_for(&net, far_uuid).expect("the host has heard from far");
    assert_eq!(
        i64::from(to_near.last().unwrap().your_margin),
        near_margin,
        "the packet to the near client carries a margin that is not its own"
    );
    assert_eq!(
        i64::from(to_far.last().unwrap().your_margin),
        far_margin,
        "the packet to the far client carries a margin that is not its own"
    );

    let near_margins: Vec<i16> = to_near.iter().map(|packet| packet.your_margin).collect();
    let far_margins: Vec<i16> = to_far.iter().map(|packet| packet.your_margin).collect();
    assert_ne!(
        near_margins, far_margins,
        "two clients on a cable and a satellite were told the same margins all along"
    );

    // Sequence numbers count per recipient.
    for (name, packets) in [("near", &to_near), ("far", &to_far)] {
        let gaps: Vec<(u32, u32)> = packets
            .windows(2)
            .filter(|pair| pair[1].seq != pair[0].seq.wrapping_add(1))
            .map(|pair| (pair[0].seq, pair[1].seq))
            .collect();
        assert!(
            gaps.is_empty(),
            "{name}: sequence numbers are not consecutive per recipient: {gaps:?}"
        );
    }
    println!(
        "near client margin {near_margin}, far client margin {far_margin}, {} and {} packets",
        to_near.len(),
        to_far.len()
    );
}

/// When a player leaves, the host forgets its margin and stops addressing it; the packets to
/// whoever stays keep carrying their own margin.
#[test]
fn a_departed_players_margin_is_removed() {
    let mut net = TickedNetwork::client_server::<Input>(2, minimal::install)
        .with_link(Link::cable())
        .with_seed(6);
    assert!(net.settle(SETTLE));
    seat_everyone(&mut net);
    let host = net.host();
    let clients = net.clients();
    let (leaver, stayer) = (clients[0], clients[1]);
    let (leaver_uuid, stayer_uuid) = (net.uuid(leaver), net.uuid(stayer));

    let both = ActionScript::new()
        .hold(0, 64, leaver_uuid, Input::RIGHT)
        .hold(0, 64, stayer_uuid, Input::LEFT);
    net.run_input_script(&both, 0);
    assert_eq!(
        host_margins(&net)
            .iter()
            .map(|(uuid, _)| *uuid)
            .collect::<Vec<_>>(),
        [leaver_uuid, stayer_uuid]
    );
    assert_eq!(recipients(&net), [leaver_uuid, stayer_uuid]);

    net.leave(leaver);
    net.trace_packets();
    let stayer_only = ActionScript::new().hold(0, 64, stayer_uuid, Input::LEFT);
    net.run_input_script(&stayer_only, 0);

    assert_eq!(
        host_margin_for(&net, leaver_uuid),
        None,
        "the host still holds a margin for a player who left"
    );
    assert_eq!(recipients(&net), [stayer_uuid]);
    assert!(
        net.snapshot_packets(host, leaver).is_empty(),
        "the host kept addressing snapshots to a client that left"
    );

    let to_stayer = net.decode_snapshots(host, stayer);
    assert!(
        to_stayer.len() >= 60,
        "the stayer stopped getting snapshots"
    );
    let stayer_margin = host_margin_for(&net, stayer_uuid).expect("the stayer is still heard");
    assert_eq!(
        i64::from(to_stayer.last().unwrap().your_margin),
        stayer_margin
    );
    assert_eq!(
        input_queue::<Input>(net.app(host)).newest_for(leaver_uuid),
        None,
        "the departed client's inputs are gone from the host's queue"
    );
}

// ── The handshake's other half ───────────────────────────────────────────────

#[test]
fn a_verified_client_gets_a_welcome_with_a_slot() {
    let mut net = TickedNetwork::client_server::<Input>(2, minimal::install)
        .with_link(Link::cable())
        .with_seed(8);
    assert!(net.settle(SETTLE));
    let host = net.host();
    let clients = net.clients();
    let (first, second) = (clients[0], clients[1]);
    let (first_uuid, second_uuid) = (net.uuid(first), net.uuid(second));

    let first_slot = slot_on(&net, first).expect("the first client was welcomed");
    let second_slot = slot_on(&net, second).expect("the second client was welcomed");
    assert_ne!(
        first_slot, second_slot,
        "two clients were given the same slot"
    );
    assert!((1..=255).contains(&first_slot) && (1..=255).contains(&second_slot));
    assert_eq!(
        slot_on(&net, host),
        Some(0),
        "the host mints as the authority"
    );

    let slots = net.app(host).world().resource::<SpawnerSlots>().clone();
    assert_eq!(slots.slot_of(first_uuid), Some(first_slot));
    assert_eq!(slots.slot_of(second_uuid), Some(second_slot));
    assert_eq!(slots.len(), 2);

    // A slot goes back when its client does, and a rejoin is welcomed again.
    net.leave(first);
    net.run(10);
    assert_eq!(
        net.app(host)
            .world()
            .resource::<SpawnerSlots>()
            .slot_of(first_uuid),
        None,
        "the slot was not freed when the client left"
    );
    assert_eq!(
        slot_on(&net, first),
        None,
        "the client kept a slot from a session it is no longer in"
    );

    net.rejoin(first);
    assert!(
        net.run_until(120, |net| slot_on(net, first).is_some()),
        "the rejoined client was never welcomed again"
    );
    let again = slot_on(&net, first).unwrap();
    assert_eq!(
        net.app(host)
            .world()
            .resource::<SpawnerSlots>()
            .slot_of(first_uuid),
        Some(again)
    );
    assert_ne!(again, second_slot);
}

/// The host addresses snapshots to clients it has verified and to nobody else: not to one
/// whose registries differ, not to one whose join is still pending.
#[test]
fn snapshots_go_to_verified_clients_only() {
    let mut net = TickedNetwork::client_server::<Input>(1, minimal::install)
        .with_link(Link::cable())
        .with_seed(10);
    let host = net.host();
    let good = net.client();
    let bad = net.add_client_built(without_pos);
    let pending = net.add_pending_client();
    net.trace_packets();
    net.run(200);

    assert!(
        !net.snapshot_packets(host, good).is_empty(),
        "the verified client got no snapshot"
    );
    assert!(
        net.snapshot_packets(host, bad).is_empty(),
        "a client whose registries differ was sent a snapshot"
    );
    assert!(
        net.snapshot_packets(host, pending).is_empty(),
        "a client whose join is not finished was sent a snapshot"
    );
    assert_eq!(recipients(&net), [net.uuid(good)]);

    // Finishing the join is what earns a snapshot.
    net.promote(pending);
    assert!(
        net.run_until(120, |net| !net.snapshot_packets(host, pending).is_empty()),
        "the promoted client never got a snapshot"
    );
    assert_eq!(recipients(&net), [net.uuid(good), net.uuid(pending)]);
    assert!(net.snapshot_packets(host, bad).is_empty());
}

// ── The bytes ────────────────────────────────────────────────────────────────

/// A client's input for the tick about to run, filed every frame the way a game's input
/// plugin does, so the host holds an input stream to relay and the bodies keep walking.
fn walk_right(
    local: Option<Res<LocalClientPlayer>>,
    tick: Res<CurrentTick>,
    mut queue: ResMut<InputQueue<Input>>,
) {
    if let Some(local) = local {
        queue.insert(tick.0 + 1, local.0, Input::RIGHT);
    }
}

/// The audit measured 1340 bytes a tick from the host to each client for two walking players
/// under the old shape. The budget here is what the wire phase promised; the figure is printed
/// so the number is on record with the test.
#[test]
fn bytes_per_tick_for_two_walking_players_is_under_600() {
    let mut net = TickedNetwork::client_server::<Input>(2, |app| {
        minimal::install(app);
        app.add_systems(PreUpdate, walk_right);
    })
    .with_link(Link::cable())
    .with_seed(12);
    assert!(net.settle(SETTLE));
    seat_everyone(&mut net);
    let host = net.host();
    let clients = net.clients();
    // Steady state: leads settled, both bodies moving, the input stream established.
    net.run(128);

    let mut figures = Vec::new();
    for client in &clients {
        let per_tick = assert_bandwidth_within(&mut net, host, *client, 256, 600);
        figures.push(per_tick);
    }
    for (client, per_tick) in clients.iter().zip(&figures) {
        println!(
            "host -> client {client:?}: {per_tick:.1} bytes per tick for two walking players \
             (audit figure under the old shape: 1340)"
        );
    }
    let report = measure_snapshot_size(&mut net, host, clients[0], 64);
    println!(
        "snapshot packets alone: {:.1} bytes mean, {} max, {} packets in 64 frames",
        report.mean_bytes, report.max_bytes, report.packets
    );
    assert!(
        report.max_bytes < 600,
        "a single snapshot packet is {} bytes",
        report.max_bytes
    );
}
