//! The input plugin over a real session: a keypress sampled on the client's tick reaches the
//! host with the client's prediction lead to spare.

use bevy::prelude::*;
use bevy_ticked_networking::input_plugin::TickedInputPlugin;
use bevy_ticked_networking::server::InputMargins;
use bevy_ticked_testing::fixtures::minimal::{self, Input, Pos, seat_everyone};
use bevy_ticked_testing::prelude::*;

const SETTLE: usize = 400;

/// The keyboard, stood in for by a resource.
#[derive(Resource, Default)]
struct Held(Option<Input>);

fn sample(held: Res<Held>) -> Option<Input> {
    held.0
}

#[test]
fn a_keypress_reaches_the_server_with_the_configured_margin() {
    let mut net = TickedNetwork::client_server::<Input>(1, |app| {
        minimal::install(app);
        app.init_resource::<Held>()
            .add_plugins(TickedInputPlugin::<Input>::new(sample));
    })
    .with_link(Link::cable())
    .with_seed(3);
    assert!(net.settle(SETTLE));
    let seats = seat_everyone(&mut net);
    net.run(60);
    let (host, client) = (net.host(), net.client());
    let uuid = net.uuid(client);
    let body = seats
        .iter()
        .find(|(u, _)| *u == uuid)
        .map(|(_, id)| *id)
        .unwrap();
    let before = latest::<Pos>(net.app(host), body).unwrap();

    // Hold the key for a second. The plugin samples it on every tick the client runs.
    net.world_mut(client).resource_mut::<Held>().0 = Some(Input::RIGHT);
    net.run(64);
    net.world_mut(client).resource_mut::<Held>().0 = None;
    net.run(16);

    let after = latest::<Pos>(net.app(host), body).unwrap();
    assert!(
        after.0 > before.0 + 40,
        "the host's body for the client moved {} over a second of held key",
        after.0 - before.0
    );
    let margin = net.app(host).world().resource::<InputMargins>().0[&uuid];
    let lead = -trails_host_by(&net, client);
    println!("client lead {lead} ticks; its inputs arrive {margin} ticks early at the host");
    assert!(margin >= 1, "the input arrived late by {} ticks", -margin);
    assert!(
        margin <= lead + 1,
        "the input arrived {margin} ticks early with a lead of {lead}: sampled for the wrong tick"
    );
    let stats = input_stats(net.app(host));
    assert_eq!(stats.late, 0, "{stats:?}");
}
