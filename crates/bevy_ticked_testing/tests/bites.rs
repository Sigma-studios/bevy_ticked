//! The harness proves it bites before anything is asserted with it.
//!
//! Every test here provokes a fault on purpose and checks the assertion that should catch it
//! does — or, for the probes, that a session where nothing happens measures nothing. A harness
//! that has never been seen to fail is a harness whose passes mean nothing.

use std::panic::{AssertUnwindSafe, catch_unwind};

use bevy::prelude::*;
use bevy_ticked::tracked_entity::TickTrackedEntityCounter;
use bevy_ticked_networking::networked_registry::NetworkedTickedAppExt;
use bevy_ticked_testing::fixtures::minimal::{
    self, EntityKind, Input, MinimalHash, Pos, Vel, seat_everyone, spawn_player,
};
use bevy_ticked_testing::prelude::*;

const SETTLE_FRAMES: usize = 300;

fn session(clients: usize) -> TickedNetwork {
    TickedNetwork::client_server::<Input>(clients, minimal::install)
}

fn settled(clients: usize) -> TickedNetwork {
    let mut net = session(clients);
    assert!(
        net.settle(SETTLE_FRAMES),
        "the session did not settle in {SETTLE_FRAMES} frames"
    );
    net
}

/// The text a panic carried, whichever way it was formatted.
fn panic_message(outcome: Result<(), Box<dyn std::any::Any + Send>>) -> String {
    let payload = outcome.expect_err("expected a panic");
    payload
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| payload.downcast_ref::<&str>().map(|s| s.to_string()))
        .unwrap_or_else(|| "<non-string panic payload>".to_string())
}

#[test]
fn one_step_is_exactly_one_tick() {
    let mut net = session(0);
    assert!(net.adopt_roles(60), "the host never adopted its role");
    // The first frame of a manual clock has no delta; give the clock a frame to start.
    net.run(2);
    let host = net.host();

    let before = tick(net.app(host));
    net.step();
    assert_eq!(
        tick(net.app(host)),
        before + 1,
        "one frame of TICK on a 64 Hz source must be exactly one tick"
    );
    assert_eq!(measure_tick_rate(&mut net, host, 32), 1.0);
}

#[test]
fn packets_actually_cross_the_link() {
    let mut net = settled(1);
    net.trace_packets();
    let (host, client) = (net.host(), net.client());
    let uuid = net.uuid(client);

    net.hold_input::<Input>(client, Input::RIGHT, 8);

    assert!(
        input_queue::<Input>(net.app(host))
            .newest_for(uuid)
            .is_some(),
        "the client's input never reached the host's queue"
    );
    assert!(
        !net.inputs_sent::<Input>(client, host).is_empty(),
        "no input packet was traced from the client to the host"
    );
    assert!(
        !net.snapshots_sent(host, client).is_empty(),
        "no snapshot was traced from the host to the client"
    );
}

#[test]
fn a_client_joins_and_settles() {
    let mut net = session(1);
    assert!(
        net.settle(SETTLE_FRAMES),
        "a client over a perfect link did not settle in {SETTLE_FRAMES} frames"
    );
    let (host, client) = (net.host(), net.client());
    assert_eq!(role(net.app(host)), Role::Host);
    assert_eq!(role(net.app(client)), Role::Client);
    assert!(!paused(net.app(client)));

    let lead = lead(net.app(client), net.app(host));
    assert!(
        (1..=64).contains(&lead),
        "a settled client leads its host by a prediction buffer, not {lead} ticks"
    );
}

#[test]
fn dropping_a_component_from_the_registry_fails_convergence() {
    let mut net = session(0);
    // A peer built from a commit that never registered `Pos`.
    net.add_client_built(|app| {
        minimal::install_systems(app);
        app.register_networked_ticked_component::<Vel>("Vel")
            .register_networked_ticked_component::<EntityKind>("EntityKind");
    });
    // The join handshake may end the session over the mismatch; whether or not it does, `Pos`
    // cannot converge on a peer that has no `Pos`.
    let _ = net.settle(120);
    let seats = seat_everyone(&mut net);
    net.run(60);

    let id = seats[1].1;
    let outcome = catch_unwind(AssertUnwindSafe(|| {
        assert_converged::<Pos>(&net, id, |a, b| (a.0 - b.0).abs() as f32, 0.0);
    }));
    let message = panic_message(outcome);
    assert!(
        message.contains("Pos"),
        "the panic did not name the component: {message}"
    );
}

#[test]
fn not_sending_client_input_fails_the_latency_measurement() {
    let mut net = settled(1);
    let seats = seat_everyone(&mut net);
    net.run(10);
    let client = net.client();
    let id = seats[1].1;
    let moved = move |host: &mut App| latest::<Pos>(host, id).is_some_and(|pos| pos.0 != 0);

    let nothing = measure_input_latency::<Input>(&mut net, client, Input::NONE, moved, 120);
    assert_eq!(
        nothing, None,
        "a press that changes nothing must not measure as latency"
    );

    let pressed = measure_input_latency::<Input>(&mut net, client, Input::RIGHT, moved, 120);
    assert!(
        pressed.is_some(),
        "a real press never showed on the host within 120 frames"
    );
}

#[test]
fn assert_all_peers_agree_fails_on_a_corrupted_replica() {
    let mut net = TickedNetwork::client_server::<Input>(1, minimal::install_with_checksums);
    assert!(net.settle(SETTLE_FRAMES));
    let (host, client) = (net.host(), net.client());
    let seats = seat_everyone(&mut net);
    // The ticks the client predicted before it learnt of the spawns are mispredictions by
    // construction; agreement starts once the snapshot carrying the bodies has arrived.
    let since = tick(net.app(client)) + 2;
    net.run(30);
    assert_all_peers_agree_since::<MinimalHash>(&net, since);
    assert!(
        !compared_ticks_since::<MinimalHash>(&net, host, client, since).is_empty(),
        "nothing was compared, so the agreement above was vacuous"
    );

    // Corrections stop, then the replica goes wrong. The client's own body: a predicted one,
    // which stays wrong. An interpolated one would be put back from the authoritative record
    // every tick, which is the point of interpolation and not of this test.
    net.disconnect(client);
    net.run(2);
    let at = tick(net.app(client));
    let id = seats[1].1;
    corrupt_component::<Pos>(net.app_mut(client), id, |pos| pos.0 += 1000);
    net.run(40);

    let outcome = catch_unwind(AssertUnwindSafe(|| {
        assert_all_peers_agree_since::<MinimalHash>(&net, since);
    }));
    let message = panic_message(outcome);
    assert!(
        message.contains(&format!("tick {}", at + 1)),
        "the panic did not name the first corrupted tick {}: {message}",
        at + 1
    );
    assert!(
        message.contains("positions"),
        "the panic did not name the differing section: {message}"
    );
}

#[test]
fn assert_no_id_unissued_fails_when_a_client_mints() {
    let mut net = settled(1);
    net.record_issued_ids();
    seat_everyone(&mut net);
    net.run(20);
    assert_no_id_unissued(&mut net);

    let client = net.client();
    let world = net.world_mut(client);
    world.resource_mut::<TickTrackedEntityCounter>().0 = 10_000;
    spawn_player(world, 99);

    let outcome = catch_unwind(AssertUnwindSafe(|| assert_no_id_unissued(&mut net)));
    let message = panic_message(outcome);
    assert!(
        message.contains("10001"),
        "the panic did not name the minted id: {message}"
    );
}

#[test]
fn garbage_packets_never_panic() {
    let mut net = settled(1);
    seat_everyone(&mut net);
    let (host, client) = (net.host(), net.client());
    let (host_uuid, client_uuid) = (net.uuid(host), net.uuid(client));

    for packet in garbage(0xF00D, 200, 96) {
        deliver_raw(&mut net, host, client_uuid, packet.clone());
        deliver_raw(&mut net, client, host_uuid, packet);
    }
    let before = (tick(net.app(host)), tick(net.app(client)));
    net.run(20);

    assert!(tick(net.app(host)) > before.0, "the host stopped ticking");
    assert!(tick(net.app(client)) > before.1, "the client stopped ticking");
}

#[test]
fn truncated_snapshot_packets_never_panic() {
    let mut net = settled(1);
    seat_everyone(&mut net);
    net.trace_packets();
    net.run(5);
    let (host, client) = (net.host(), net.client());
    let host_uuid = net.uuid(host);

    let snapshot = net
        .snapshot_packets(host, client)
        .into_iter()
        .max_by_key(|packet| packet.bytes.len())
        .expect("a snapshot crossed the link")
        .bytes;
    assert!(snapshot.len() > 8, "the traced snapshot is implausibly small");

    for prefix in truncations(&snapshot) {
        deliver_raw(&mut net, client, host_uuid, prefix.to_vec());
    }
    for flipped in bit_flips(&snapshot, 0xBEEF, 64) {
        deliver_raw(&mut net, client, host_uuid, flipped);
    }
    let before = tick(net.app(client));
    net.run(20);

    assert!(tick(net.app(client)) > before, "the client stopped ticking");
}
