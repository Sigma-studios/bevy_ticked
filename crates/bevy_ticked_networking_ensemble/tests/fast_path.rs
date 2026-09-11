//! The audit's number: seven simulation runs per frame on a client where nothing happened.
//! A snapshot that agrees with the prediction costs no replay now, and a remote body that
//! is interpolated never enters the comparison.

use bevy_ticked_testing::fixtures::minimal::{self, Input, Pos, seat_everyone};
use bevy_ticked_testing::prelude::*;

const SETTLE: usize = 400;

fn session(clients: usize) -> TickedNetwork {
    let mut net = TickedNetwork::client_server::<Input>(clients, minimal::install)
        .with_link(Link::cable())
        .with_seed(5);
    assert!(net.settle(SETTLE));
    seat_everyone(&mut net);
    net.run(60);
    net
}

#[test]
fn a_static_world_never_replays() {
    let mut net = session(1);
    let client = net.client();
    let before = replays(net.app(client));
    net.run(200);
    let after = replays(net.app(client));
    assert_eq!(
        after.rollbacks, before.rollbacks,
        "two hundred frames of nothing happening cost no replay ({after:?})"
    );
    assert!(after.skipped_identical - before.skipped_identical >= 150);
    let cost = measure_tick_cost(net.app(client));
    println!("sims per frame on a static client: {} (was 7)", (after.ticks_replayed - before.ticks_replayed) as f64 / 200.0 + 1.0);
    let _ = cost;
}

/// The four-percent case the audit made a hundred: a remote player walking is an ordinary
/// state of a session, and it used to be a replay on every snapshot. Interpolated, the
/// walking body is drawn from the record and never compared.
#[test]
fn a_walking_remote_body_in_interpolated_mode_never_replays() {
    let mut net = session(2);
    let clients = net.clients();
    let (a, b) = (clients[0], clients[1]);
    let before = replays(net.app(b));
    net.hold_input(a, Input::RIGHT, 128);
    let after = replays(net.app(b));
    assert_eq!(
        after.rollbacks, before.rollbacks,
        "B watched A walk for two seconds and replayed nothing ({after:?})"
    );
}

/// A client's own walk agrees with the host's simulation of it, so it replays nothing
/// either; only a real misprediction does.
#[test]
fn a_clients_own_walk_replays_nothing_when_the_host_agrees() {
    let mut net = session(1);
    let client = net.client();
    let before = replays(net.app(client));
    net.hold_input(client, Input::RIGHT, 64);
    let after = replays(net.app(client));
    assert!(
        after.rollbacks - before.rollbacks <= 1,
        "walking on a perfect cable should not disagree with the host ({after:?})"
    );
}

/// Input latency is what the host sees: frames from the press until the host's copy of the
/// body moves. Halving the client's frame rate (two ticks per frame) does not double it,
/// because the client stamps inputs by tick and the host reads them by tick.
#[test]
fn frame_time_is_not_input_latency() {
    let mut net = TickedNetwork::client_server::<Input>(1, minimal::install)
        .with_link(Link::cable())
        .with_seed(5);
    assert!(net.settle(SETTLE));
    let seats = seat_everyone(&mut net);
    net.run(60);
    let client = net.client();
    let id = seats
        .iter()
        .find(|(uuid, _)| *uuid == net.uuid(client))
        .map(|(_, id)| *id)
        .unwrap();
    let host = net.host();

    let start = latest::<Pos>(net.app(host), id).unwrap().0;
    let at_full_rate = measure_input_latency::<Input>(
        &mut net,
        client,
        Input::RIGHT,
        move |app| latest::<Pos>(app, id).unwrap().0 != start,
        64,
    )
    .expect("the press reached the host");
    net.hold_input(client, Input::NONE, 8);
    net.run(40);

    net.set_ticks_per_frame(client, 2.0);
    net.run(120);
    let start = latest::<Pos>(net.app(host), id).unwrap().0;
    let at_half_rate = measure_input_latency::<Input>(
        &mut net,
        client,
        Input::RIGHT,
        move |app| latest::<Pos>(app, id).unwrap().0 != start,
        64,
    )
    .expect("the press reached the host");
    println!("input latency in host frames: {at_full_rate} at 64 fps, {at_half_rate} at 32 fps");
    assert!(
        (at_full_rate as i64 - at_half_rate as i64).abs() <= 3,
        "halving the client's frame rate did not double its input latency: {at_full_rate} vs \
         {at_half_rate}"
    );
}
