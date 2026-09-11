//! A client-server session over every link a player has, in one process.
//!
//! Ported from run-2d's `net_tests.rs`, which ran its own game over the loopback backend and
//! asserted convergence rather than bit-equality. Here the game is the harness's integer
//! fixture, so "converged" is exact: a client that has been told the host's world holds the
//! same integers. What is under test is the whole path — snapshot, input, prediction, rollback,
//! role adoption — over delay, jitter, loss, duplication and reordering.

use std::time::Duration;

use bevy_ticked::prelude::*;
use bevy_ticked_testing::fixtures::minimal::{self, Input, Owner, Pos, seat_everyone};
use bevy_ticked_testing::prelude::*;

const SETTLE: usize = 400;

fn session(clients: usize, link: Link) -> TickedNetwork {
    let mut net = TickedNetwork::client_server::<Input>(clients, minimal::install)
        .with_link(link)
        .with_seed(7);
    assert!(
        net.settle(SETTLE),
        "the session did not settle in {SETTLE} frames over {link:?}"
    );
    net
}

fn exact(a: &Pos, b: &Pos) -> f32 {
    (a.0 - b.0).abs() as f32
}

fn body_of(seats: &[(u128, u64)], uuid: u128) -> u64 {
    seats
        .iter()
        .find(|(owner, _)| *owner == uuid)
        .map(|(_, id)| *id)
        .expect("every peer was seated")
}

/// Hold `input` on `peer` for `frames` frames and return how many ticks that covered.
///
/// Not always `frames`: a predicting client dilates its clock, and a frame that runs no tick
/// queues its input for the same tick as the frame before. The body moves once per *tick* the
/// key was down, so that is the number to expect on the host.
fn hold(net: &mut TickedNetwork, peer: PeerId, input: Input, frames: usize) -> i64 {
    let before = tick(net.app(peer));
    net.hold_input(peer, input, frames);
    (tick(net.app(peer)) - before) as i64
}

/// Every body agrees on every client, once the link has had time to carry the last word.
fn assert_everyone_converged(net: &TickedNetwork, seats: &[(u128, u64)]) {
    for (_, id) in seats {
        assert_converged::<Pos>(net, *id, exact, 0.0);
    }
}

// ── Presets ──────────────────────────────────────────────────────────────────

#[test]
fn a_client_converges_over_every_preset() {
    for (name, link) in [
        ("perfect", Link::perfect()),
        ("cable", Link::cable()),
        ("four_g", Link::four_g()),
        ("bad_wifi", Link::bad_wifi()),
        ("satellite", Link::satellite()),
    ] {
        let mut net = session(1, link);
        let seats = seat_everyone(&mut net);
        let client = net.client();
        net.run(30);
        let held = hold(&mut net, client, Input::RIGHT, 64);
        net.hold_input(client, Input::NONE, 4);
        net.run(200);

        let host_pos = latest::<Pos>(net.app(net.host()), body_of(&seats, net.uuid(client)))
            .map(|pos| pos.0)
            .expect("the host has the body");
        // A release that arrives late is applied late, and the key is held until then: a
        // tick or two of extra motion on a slow link, where a lost keypress was the
        // alternative.
        assert!(
            (held..=held + 2).contains(&host_pos),
            "{name}: the key was down for {held} ticks; the host moved the body {host_pos}"
        );
        assert_everyone_converged(&net, &seats);
        println!("{name}: converged, host body at {host_pos:?}");
    }
}

// ── Input over bad links ─────────────────────────────────────────────────────

/// Inputs are sent with the recent ticks repeated, so a lost datagram loses nothing, and a
/// late one is used late rather than dropped. Sixty-four held ticks move the body sixty-four
/// units on the host, whatever the wire did.
#[test]
fn input_survives_a_jittery_link() {
    let mut net = session(1, Link::bad_wifi().with_jitter(Duration::from_millis(60)));
    let seats = seat_everyone(&mut net);
    let (host, client) = (net.host(), net.client());
    let id = body_of(&seats, net.uuid(client));
    net.run(30);

    let held = hold(&mut net, client, Input::RIGHT, 64);
    net.hold_input(client, Input::NONE, 8);
    net.run(300);

    let pos = latest::<Pos>(net.app(host), id).expect("the host has the body");
    assert_eq!(
        pos.0, held,
        "{held} ticks of RIGHT should move the body {held} units on the host; inputs were lost \
         or applied twice"
    );
    assert_everyone_converged(&net, &seats);
}

#[test]
fn a_laggy_client_can_move() {
    let mut net = session(1, Link::satellite());
    let seats = seat_everyone(&mut net);
    let (host, client) = (net.host(), net.client());
    let id = body_of(&seats, net.uuid(client));
    net.run(30);

    let held = hold(&mut net, client, Input::RIGHT, 64);
    net.hold_input(client, Input::NONE, 8);
    net.run(400);

    let pos = latest::<Pos>(net.app(host), id).expect("the host has the body");
    assert!(
        (held..=held + 2).contains(&pos.0),
        "a 600 ms round trip is late, not lossy: held {held}, moved {}",
        pos.0
    );
    assert_everyone_converged(&net, &seats);
    let lead = lead(net.app(client), net.app(host));
    assert!(
        lead >= 20,
        "a client on a satellite link must lead by most of the round trip; leads by {lead}"
    );
}

/// Duplicated input packets carry the same ticks twice; the queue keeps one per tick and the
/// body moves once per tick.
#[test]
fn duplicated_inputs_do_not_double_apply() {
    let mut net = session(1, Link::cable().with_duplicate(1.0));
    let seats = seat_everyone(&mut net);
    let (host, client) = (net.host(), net.client());
    let id = body_of(&seats, net.uuid(client));
    net.run(30);

    let held = hold(&mut net, client, Input::RIGHT, 64);
    net.hold_input(client, Input::NONE, 8);
    net.run(200);

    let pos = latest::<Pos>(net.app(host), id).expect("the host has the body");
    assert_eq!(pos.0, held);
    let stats = input_stats(net.app(host));
    assert!(
        stats.received as i64 > held,
        "the host should have seen every input at least twice ({stats:?})"
    );
    assert_everyone_converged(&net, &seats);
}

/// The prediction lead follows the link: a client whose round trip jumps from a cable's to a
/// satellite's grows its lead until its inputs arrive in time again, and shrinks it back when
/// the link recovers. Measured from the server's margin reports, which is the only signal a
/// client has for it — so the client has to be sending input, as a game's input plugin does
/// every tick whether or not a key is down.
#[test]
fn the_buffer_converges_after_a_step_change_in_latency() {
    let mut net = session(1, Link::cable());
    seat_everyone(&mut net);
    let (host, client) = (net.host(), net.client());
    net.hold_input(client, Input::NONE, 120);
    let on_cable = target_replay_distance(net.app(client));

    net.net.set_link(Link::satellite());
    net.hold_input(client, Input::NONE, 600);
    let on_satellite = target_replay_distance(net.app(client));
    assert!(
        on_satellite >= on_cable + 15,
        "a 600 ms round trip needs about 40 more ticks of lead; the target went from {on_cable} \
         to {on_satellite}"
    );
    let margin = target_margin(net.app(client));
    let stats = input_stats(net.app(host));
    assert!(
        stats.late < stats.received / 4,
        "inputs kept arriving late after the lead grew: {stats:?}, target margin {margin}"
    );

    net.net.set_link(Link::cable());
    net.hold_input(client, Input::NONE, 900);
    let back = target_replay_distance(net.app(client));
    assert!(
        back <= on_cable + 4,
        "the lead never came back down after the link recovered: {on_cable} -> {on_satellite} \
         -> {back}"
    );
}

// ── Snapshots over bad links ─────────────────────────────────────────────────

#[test]
fn duplicated_snapshots_are_applied_once() {
    let mut net = session(1, Link::cable().with_duplicate(1.0));
    let seats = seat_everyone(&mut net);
    let client = net.client();
    replays(net.app(client));
    net.app_mut(client)
        .world_mut()
        .resource_mut::<bevy_ticked_networking::diagnostics::ReplayStats>()
        .reset();

    net.run(100);

    let stats = replays(net.app(client));
    assert!(
        stats.snapshots_applied <= 100,
        "more snapshots applied than frames run: a duplicate was applied ({stats:?})"
    );
    assert!(
        stats.dropped_stale > 0,
        "every snapshot was duplicated and none was seen as stale ({stats:?})"
    );
    assert_everyone_converged(&net, &seats);
}

/// Three frames of delay, so that consecutive snapshots are in flight together: the loopback
/// overtakes by swapping two packets that are both still on the link, and on a cable every
/// unreliable packet lands on the next frame with nothing to swap with. Over a plain cable
/// this test passed on the one stale drop every session starts with — the duplicate tick-1
/// snapshot — which lands before or after the counter reset depending on the wall-clock ping
/// the client seeds its lead from, and so on machine load.
#[test]
fn a_reordered_snapshot_is_dropped_over_the_link() {
    let mut net = session(
        1,
        Link::cable()
            .with_delay(Duration::from_millis(45))
            .with_reorder(0.5),
    );
    let seats = seat_everyone(&mut net);
    let client = net.client();
    net.app_mut(client)
        .world_mut()
        .resource_mut::<bevy_ticked_networking::diagnostics::ReplayStats>()
        .reset();

    net.run(200);

    let stats = replays(net.app(client));
    assert!(
        stats.dropped_stale >= 20,
        "half the snapshots were swapped with their neighbour and almost none was dropped as \
         stale ({stats:?})"
    );
    let applied = applied_tick(net.app(client)).expect("a snapshot was applied");
    assert!(applied > 0);
    assert_everyone_converged(&net, &seats);
}

#[test]
fn asymmetric_links_converge() {
    let mut net = TickedNetwork::client_server::<Input>(1, minimal::install).with_seed(3);
    let (host, client) = (net.host(), net.client());
    net.net
        .set_link_pair(host, client, Link::satellite(), Link::cable());
    assert!(net.settle(SETTLE));
    let seats = seat_everyone(&mut net);
    net.run(30);

    let held = hold(&mut net, client, Input::LEFT, 32);
    net.hold_input(client, Input::NONE, 8);
    net.run(300);

    let id = body_of(&seats, net.uuid(client));
    assert_eq!(latest::<Pos>(net.app(host), id).unwrap().0, -held);
    assert_everyone_converged(&net, &seats);
}

// ── Several peers ────────────────────────────────────────────────────────────

#[test]
fn three_clients_on_a_rollback_stack_agree_about_the_roster() {
    let mut net = session(3, Link::four_g());
    let seats = seat_everyone(&mut net);
    net.run(60);

    let mut expected: Vec<u128> = seats.iter().map(|(uuid, _)| *uuid).collect();
    expected.sort_unstable();
    for peer in net.peers() {
        let mut ids = tracked_ids(net.app_mut(peer));
        ids.sort_unstable();
        let mut owners: Vec<u128> = ids
            .iter()
            .filter_map(|id| latest::<Owner>(net.app(peer), *id).map(|owner| owner.0))
            .collect();
        owners.sort_unstable();
        assert_eq!(
            owners,
            expected,
            "peer {peer:?} (uuid {}) sees a different roster",
            net.uuid(peer)
        );
    }

    for client in net.clients() {
        net.hold_input(client, Input::RIGHT, 16);
        net.hold_input(client, Input::NONE, 2);
    }
    net.run(200);
    assert_everyone_converged(&net, &seats);
}

#[test]
fn peers_at_different_frame_rates_converge() {
    let mut net = session(2, Link::cable());
    let seats = seat_everyone(&mut net);
    let clients = net.clients();
    // One client renders at 128 fps, the other at 32: half a tick and two ticks per frame.
    net.set_ticks_per_frame(clients[0], 0.5);
    net.set_ticks_per_frame(clients[1], 2.0);
    net.run(100);

    net.hold_input(clients[0], Input::RIGHT, 40);
    net.hold_input(clients[0], Input::NONE, 4);
    net.hold_input(clients[1], Input::LEFT, 20);
    net.hold_input(clients[1], Input::NONE, 4);
    net.run(300);
    for client in &clients {
        let lead = lead(net.app(*client), net.app(net.host()));
        assert!(
            (1..=40).contains(&lead),
            "a peer at another frame rate still leads by a buffer, not {lead}"
        );
    }

    let host = net.host();
    let fast = latest::<Pos>(net.app(host), body_of(&seats, net.uuid(clients[0]))).unwrap();
    let slow = latest::<Pos>(net.app(host), body_of(&seats, net.uuid(clients[1]))).unwrap();
    assert!(
        fast.0 > 0 && slow.0 < 0,
        "both bodies moved on the host: {fast:?} {slow:?}"
    );
    assert_everyone_converged(&net, &seats);
}

// ── A long session ───────────────────────────────────────────────────────────

/// Ten thousand ticks over a bad link, then a look at what the client is still holding.
#[test]
fn a_ten_thousand_tick_session_keeps_history_and_queues_bounded() {
    let mut net = session(1, Link::bad_wifi());
    let seats = seat_everyone(&mut net);
    let (host, client) = (net.host(), net.client());
    let window = net.app(client).world().resource::<HistoryBufferTicks>().0;

    for round in 0..100 {
        let input = if round % 2 == 0 {
            Input::RIGHT
        } else {
            Input::LEFT
        };
        net.hold_input(client, input, 100);
    }
    net.hold_input(client, Input::NONE, 4);
    net.run(200);

    let (oldest, newest) = history_range::<Pos>(net.app(client)).expect("the client has history");
    assert!(
        newest - oldest <= window,
        "the client holds {} ticks of Pos history, window {window}",
        newest - oldest
    );
    let queued = input_queue::<Input>(net.app(host)).ticks().len() as u64;
    assert!(
        queued <= window + 64,
        "the host holds {queued} ticks of input; history window {window}"
    );
    assert_everyone_converged(&net, &seats);
}
