//! The same seed, peers and inputs replay the same session.
//!
//! What makes a lossy-link failure reproducible. Compares what the link did (every packet's
//! frame, endpoints and fate) and where the bodies ended up.
//!
//! Not compared, and each a finding:
//! - packet *bytes*: the snapshot used to be a `HashMap` on the wire, and two encodings of the
//!   same world could differ in order. The wire phase sorted it; what still keeps the bytes
//!   from being compared is the next item;
//! - packet *sizes*: a pong carries a wall-clock dwell, and the client seeds its prediction
//!   lead from the ping round trip, which over loopback is the wall clock too. A run's lead
//!   therefore differs by a tick or so between runs, and with it the tick every input is
//!   stamped with and the margin every snapshot carries. The outcome is the same; the trace is
//!   not byte-for-byte. A loopback clock that pings by the frame would close this.

use bevy_ticked_testing::fixtures::minimal::{self, Input, Pos, seat_everyone};
use bevy_ticked_testing::prelude::*;

struct Run {
    fates: Vec<(u64, PeerId, PeerId, PacketFate)>,
    sizes: Vec<usize>,
    positions: Vec<Option<Pos>>,
}

fn play(seed: u64) -> Run {
    let mut net = TickedNetwork::client_server::<Input>(2, minimal::install)
        .with_link(Link::bad_wifi())
        .with_seed(seed);
    assert!(net.settle(400));
    let seats = seat_everyone(&mut net);
    net.trace_packets();
    let clients = net.clients();
    let script = ActionScript::new()
        .hold(0, 40, net.uuid(clients[0]), Input::RIGHT)
        .hold(10, 50, net.uuid(clients[1]), Input::LEFT)
        .at(41, net.uuid(clients[0]), Input::NONE)
        .at(51, net.uuid(clients[1]), Input::NONE);
    net.run_input_script(&script, 200);

    let fates = net
        .trace()
        .iter()
        .map(|p| (p.frame, p.from, p.to, p.fate))
        .collect();
    let mut sizes = Vec::new();
    for from in net.peers() {
        for to in net.peers() {
            sizes.extend(net.snapshots_sent(from, to).iter().map(|s| s.bytes));
            sizes.extend(net.inputs_sent::<Input>(from, to).iter().map(|s| s.bytes));
        }
    }
    let positions = net
        .peers()
        .into_iter()
        .flat_map(|peer| seats.iter().map(move |(_, id)| (peer, *id)))
        .map(|(peer, id)| latest::<Pos>(net.app(peer), id))
        .collect();
    Run {
        fates,
        sizes,
        positions,
    }
}

#[test]
fn the_same_seed_replays_the_same_trace() {
    let a = play(0xC0FFEE);
    let b = play(0xC0FFEE);
    assert!(!a.fates.is_empty());
    assert_eq!(
        a.fates.len(),
        b.fates.len(),
        "the two runs sent a different number of packets"
    );
    for (i, (x, y)) in a.fates.iter().zip(&b.fates).enumerate() {
        assert_eq!(x, y, "packet {i} differs between two runs of the same seed");
    }
    assert_eq!(
        a.sizes.len(),
        b.sizes.len(),
        "a different number of ticked packets crossed"
    );
    assert_eq!(a.positions, b.positions);
    assert!(a.positions.iter().all(|pos| pos.is_some()));
}

#[test]
fn a_different_seed_is_a_different_session() {
    let a = play(1);
    let b = play(2);
    assert_ne!(
        a.fates, b.fates,
        "two seeds produced the same packet fates over a 2% loss link; the seed is not used"
    );
}
