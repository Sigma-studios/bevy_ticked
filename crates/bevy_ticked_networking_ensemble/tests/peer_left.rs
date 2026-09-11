//! A client that leaves is forgotten by the host, through the bridge.
//!
//! `bevy_ticked_networking` learns of a departure from [`PeerLeft`], and the only place that
//! knows about departures is the lobby crate, so the bridge writes it when a `LobbyClient` is
//! removed on the host. Over the harness that is `net.leave`, which does what every backend
//! does: despawns the client's entity on the host.
//!
//! [`PeerLeft`]: bevy_ticked_networking::messages::PeerLeft

use bevy_ticked_networking::server::InputMargins;
use bevy_ticked_testing::fixtures::minimal::{self, Input, seat_everyone};
use bevy_ticked_testing::prelude::*;

const SETTLE: usize = 400;

fn has_margin_for(net: &TickedNetwork, uuid: u128) -> bool {
    net.app(net.host())
        .world()
        .resource::<InputMargins>()
        .0
        .contains_key(&uuid)
}

#[test]
fn a_client_that_leaves_is_forgotten_by_the_host() {
    let mut net = TickedNetwork::client_server::<Input>(2, minimal::install)
        .with_link(Link::cable())
        .with_seed(3);
    assert!(net.settle(SETTLE), "the session did not settle");
    seat_everyone(&mut net);
    let clients = net.clients();
    let (leaver, stayer) = (clients[0], clients[1]);
    let (leaver_uuid, stayer_uuid) = (net.uuid(leaver), net.uuid(stayer));

    net.hold_input(leaver, Input::RIGHT, 32);
    net.hold_input(stayer, Input::LEFT, 32);
    net.run(30);
    assert!(
        has_margin_for(&net, leaver_uuid) && has_margin_for(&net, stayer_uuid),
        "both clients have been heard from before one of them goes"
    );

    net.leave(leaver);
    net.run(10);

    assert!(
        !has_margin_for(&net, leaver_uuid),
        "the host still reports a margin for a client that left, so every snapshot carries it"
    );
    assert!(
        has_margin_for(&net, stayer_uuid),
        "and the client that stayed is still there"
    );
    assert_eq!(
        input_queue::<Input>(net.app(net.host())).newest_for(leaver_uuid),
        None,
        "the departed client's inputs are gone from the host's queue"
    );
}
