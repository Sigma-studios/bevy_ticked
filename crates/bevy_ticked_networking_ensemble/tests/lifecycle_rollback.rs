//! A client that predicts a spawn, and a host that confirms it under the same id.
//!
//! Ported from run-2d's grenade tests. The pellet is the harness fixture's: fired by a player,
//! minted under that player's slot on every peer, tombstoned when its fuse burns down.

use bevy::prelude::*;
use bevy_ticked::lifetimes::TickedEntityCommandsExt;
use bevy_ticked::tracked_entity::{SpawnerSlot, TickTrackedEntity, TrackedWorldExt};
use bevy_ticked_testing::fixtures::minimal::{
    self, EntityKind, Fuse, Input, Pos, Vel, seat_everyone,
};
use bevy_ticked_testing::prelude::*;

const SETTLE: usize = 400;

struct Session {
    net: TickedNetwork,
    host: PeerId,
    a: PeerId,
    b: PeerId,
    seats: Vec<(u128, u64)>,
}

fn session(link: Link) -> Session {
    let mut net = TickedNetwork::client_server::<Input>(2, minimal::install)
        .with_link(link)
        .with_seed(9);
    assert!(net.settle(SETTLE));
    let seats = seat_everyone(&mut net);
    net.run(60);
    let clients = net.clients();
    Session {
        host: net.host(),
        a: clients[0],
        b: clients[1],
        net,
        seats,
    }
}

fn pellets(app: &mut App) -> Vec<u64> {
    let mut q = app.world_mut().query::<(&TickTrackedEntity, &EntityKind)>();
    let mut ids: Vec<u64> = q
        .iter(app.world())
        .filter(|(_, kind)| **kind == EntityKind::PELLET)
        .map(|(t, _)| t.0)
        .collect();
    ids.sort_unstable();
    ids
}

fn press(net: &mut TickedNetwork, peer: PeerId, input: Input) {
    let uuid = net.uuid(peer);
    queue_input(net.app_mut(peer), uuid, input);
    net.step();
}

fn slot_of(net: &TickedNetwork, peer: PeerId) -> SpawnerSlot {
    net.app(peer)
        .world()
        .resource::<bevy_ticked::tracked_entity::LocalSpawnerSlot>()
        .0
}

#[test]
fn a_predicted_bullet_survives_the_snapshot_that_predates_it() {
    let Session {
        mut net, host, a, ..
    } = session(Link::cable());
    press(&mut net, a, Input::FIRE);
    let fired = pellets(net.app_mut(a));
    assert_eq!(
        fired.len(),
        1,
        "the pellet is there the frame the key went down"
    );
    assert_eq!(TickTrackedEntity(fired[0]).slot(), slot_of(&net, a));

    let mut host_has_it_at = None;
    for frame in 0..40 {
        press(&mut net, a, Input::NONE);
        assert_eq!(
            pellets(net.app_mut(a)),
            fired,
            "frame {frame}: a snapshot from before the shot took the pellet away"
        );
        if host_has_it_at.is_none() && pellets(net.app_mut(host)) == fired {
            host_has_it_at = Some(frame);
        }
    }
    assert!(
        host_has_it_at.is_some(),
        "the host minted the same id from the relayed input"
    );
}

#[test]
fn a_mispredicted_spawn_disappears_when_the_authority_never_confirms_it() {
    let Session { mut net, a, .. } = session(Link::cable());
    let slot = slot_of(&net, a);
    // From outside the simulation: the host never runs this spawn.
    net.world_mut(a)
        .spawn_tracked_by(slot, (Pos(0), Vel(0), EntityKind::PELLET, Fuse(32)));
    assert_eq!(pellets(net.app_mut(a)).len(), 1);
    net.run(40);
    assert!(
        pellets(net.app_mut(a)).is_empty(),
        "the authority never had it"
    );
    assert_eq!(
        tombstone_count(net.app(a)),
        1,
        "tombstoned, not destroyed, in case a later word brings it back"
    );
}

#[test]
fn a_predicted_despawn_the_host_contradicts_is_undone() {
    let Session {
        mut net,
        host,
        a,
        seats,
        ..
    } = session(Link::cable());
    let host_body = seats
        .iter()
        .find(|(uuid, _)| *uuid == net.uuid(host))
        .map(|(_, id)| *id)
        .unwrap();
    let entity = tracked_entity(net.app(a), host_body).unwrap();
    net.world_mut(a).entity_mut(entity).despawn_ticked();
    assert!(tracked_entity(net.app(a), host_body).is_none());
    net.run(10);
    assert_eq!(
        tracked_entity(net.app(a), host_body),
        Some(entity),
        "the host still has its body, so the same entity is back"
    );
}

#[test]
fn two_clients_spawning_in_the_same_tick_never_collide_on_ids() {
    let Session {
        mut net,
        host,
        a,
        b,
        ..
    } = session(Link::cable());
    let (ua, ub) = (net.uuid(a), net.uuid(b));
    queue_input(net.app_mut(a), ua, Input::FIRE);
    queue_input(net.app_mut(b), ub, Input::FIRE);
    net.step();
    net.run(40);
    let on_host = pellets(net.app_mut(host));
    assert_eq!(on_host.len(), 2);
    let slots: Vec<SpawnerSlot> = on_host
        .iter()
        .map(|id| TickTrackedEntity(*id).slot())
        .collect();
    assert_ne!(slots[0], slots[1], "each pellet under its shooter's slot");
    assert_eq!(pellets(net.app_mut(a)), on_host);
    assert_eq!(pellets(net.app_mut(b)), on_host);
}

#[test]
fn a_client_never_mints_an_id_the_host_will_reuse() {
    let Session { mut net, a, b, .. } = session(Link::four_g());
    for frame in 0..200 {
        for peer in [a, b] {
            let uuid = net.uuid(peer);
            let input = if frame % 8 == 0 {
                Input::FIRE
            } else {
                Input::NONE
            };
            queue_input(net.app_mut(peer), uuid, input);
        }
        net.step();
    }
    for peer in net.peers() {
        assert_ids_unique(net.app_mut(peer));
    }
    for peer in [a, b] {
        assert_eq!(
            health(net.app(peer)).client_minted_tracked_id,
            0,
            "a client minting under its own slot is not a client minting the authority's"
        );
    }
}

#[test]
fn on_add_fires_once_for_a_predicted_spawn_confirmed_by_the_host() {
    #[derive(Resource, Default)]
    struct Added(Vec<u64>);
    let Session { mut net, a, .. } = session(Link::cable());
    net.app_mut(a).init_resource::<Added>().add_observer(
        |add: On<Add, TickTrackedEntity>,
         tracked: Query<&TickTrackedEntity>,
         mut added: ResMut<Added>| {
            if let Ok(t) = tracked.get(add.entity) {
                added.0.push(t.0);
            }
        },
    );
    press(&mut net, a, Input::FIRE);
    let id = pellets(net.app_mut(a))[0];
    net.run(40);
    let times = net
        .app(a)
        .world()
        .resource::<Added>()
        .0
        .iter()
        .filter(|added| **added == id)
        .count();
    assert_eq!(
        times, 1,
        "predicted once, confirmed onto the same entity, added once"
    );
}

#[test]
fn a_spawn_survives_a_dropped_snapshot() {
    let Session {
        mut net,
        host,
        a,
        b,
        ..
    } = session(Link::cable().with_loss(0.3));
    press(&mut net, a, Input::FIRE);
    let fired = pellets(net.app_mut(a));
    assert_eq!(fired.len(), 1);
    net.run(64);
    assert_eq!(pellets(net.app_mut(host)), fired, "the host has it");
    assert_eq!(
        pellets(net.app_mut(b)),
        fired,
        "and so does the other client"
    );
}

/// run-2d's grenade: fired by A, watched on A every frame from appearance to expiry, never
/// absent for a frame.
#[test]
fn a_grenade_does_not_blink_on_the_client() {
    let Session { mut net, a, .. } = session(Link::bad_wifi());
    press(&mut net, a, Input::FIRE);
    let fired = pellets(net.app_mut(a));
    assert_eq!(fired.len(), 1);
    let mut lifetime = 0;
    for frame in 0..(minimal::PELLET_LIFE as usize - 8) {
        press(&mut net, a, Input::NONE);
        assert_eq!(
            pellets(net.app_mut(a)),
            fired,
            "frame {frame}: the pellet blinked"
        );
        lifetime += 1;
    }
    assert!(lifetime > 20);
    net.run(48);
    assert!(pellets(net.app_mut(a)).is_empty(), "and it expires on time");
}
