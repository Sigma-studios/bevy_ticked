//! Delta replication: the part of the bandwidth design the wire phase reserved a variant for.
//!
//! The host keeps the last few packets it sent each client and, once the client acknowledges
//! one, sends only what changed since it. A client rebuilds the whole body from the baseline
//! before anything downstream sees it, so the fast path, the rollback and the drawn history
//! never learn that a delta existed. The numbers the audit measured (1340 bytes a tick for two
//! walking players) end here.

use bevy::prelude::*;
use bevy_ticked::lifetimes::TickedEntityCommandsExt;
use bevy_ticked::registry::TickedComponentRegistry;
use bevy_ticked::tracked_entity::TrackedWorldExt;
use bevy_ticked_networking::delta::{
    Baseline, Compression, DeltaPolicy, SendRates, apply_delta, build_delta, layouts_of,
};
use bevy_ticked_networking::server::SnapshotCompression;
use bevy_ticked_networking::snapshot::{SnapshotBody, build_full_body};
use bevy_ticked_networking_ensemble::EnsembleInputMessage;
use bevy_ticked_testing::fixtures::minimal::{self, EntityKind, Input, Pos, Vel, seat_everyone};
use bevy_ticked_testing::prelude::*;

const SETTLE: usize = 400;

/// A session with `clients` clients and `statics` motionless tracked entities on the host.
fn session_with(
    clients: usize,
    statics: usize,
    seed: u64,
    link: Link,
    build: fn(&mut App),
) -> TickedNetwork {
    let mut net = TickedNetwork::client_server::<Input>(clients, move |app| {
        minimal::install(app);
        build(app);
    })
    .with_link(link)
    .with_seed(seed);
    let host = net.host();
    for i in 0..statics {
        net.world_mut(host)
            .spawn_tracked((Pos(100 + i as i64), Vel(0), EntityKind(9)));
    }
    assert!(net.settle(SETTLE));
    seat_everyone(&mut net);
    net.run(60);
    net
}

fn session(clients: usize, statics: usize) -> TickedNetwork {
    session_with(clients, statics, 13, Link::cable(), |_| {})
}

/// The motionless entities: a walking player on a client is predicted ahead of the host by
/// its lead, and is compared through the fast path rather than by position.
fn static_ids(net: &mut TickedNetwork) -> Vec<u64> {
    let host = net.host();
    tracked_ids(net.app_mut(host))
        .into_iter()
        .filter(|id| latest::<EntityKind>(net.app(host), *id) == Some(EntityKind(9)))
        .collect()
}

fn deltas_of(net: &TickedNetwork, from: PeerId, to: PeerId) -> usize {
    net.decode_snapshots(from, to)
        .iter()
        .filter(|packet| matches!(packet.body, SnapshotBody::Delta(_)))
        .count()
}

// ── The transform itself ─────────────────────────────────────────────────────

/// Pure: a delta built from two bodies, applied to the first, is the second.
#[test]
fn a_delta_against_an_acked_baseline_reproduces_the_full_state() {
    let mut net = session(1, 20);
    let host = net.host();
    let registry = net
        .app(host)
        .world()
        .resource::<TickedComponentRegistry>()
        .clone();
    let tick = bevy_ticked_testing::view::tick(net.app(host));
    let before = build_full_body(net.world_mut(host), tick);
    let layouts = layouts_of(&registry, &before);
    // Something moved, something lost a component, something died, something was born.
    let ids: Vec<u64> = before.ids().collect();
    let moved = tracked_entity(net.app(host), ids[3]).unwrap();
    let stripped = tracked_entity(net.app(host), ids[4]).unwrap();
    let dead = tracked_entity(net.app(host), ids[5]).unwrap();
    {
        let world = net.world_mut(host);
        world.get_mut::<Pos>(moved).unwrap().0 += 7;
        world.entity_mut(stripped).remove::<Vel>();
        world.entity_mut(dead).despawn_ticked();
        world.spawn_tracked((Pos(-1), Vel(0), EntityKind(3)));
    }
    // A tick, so the changes are in the history a body is built from.
    net.step();
    let after_tick = bevy_ticked_testing::view::tick(net.app(host));
    assert!(after_tick > tick);
    let after = build_full_body(net.world_mut(host), after_tick);
    let after_layouts = layouts_of(&registry, &after);
    let baseline = Baseline {
        seq: 1,
        tick,
        body: before.clone(),
        layouts,
    };
    let delta = build_delta(
        &registry,
        &after,
        &after_layouts,
        &baseline,
        &SendRates::default(),
        2,
    );
    assert_eq!(delta.despawned, vec![ids[5]]);
    assert_eq!(
        delta.removed.len(),
        1,
        "one component removal: {:?}",
        delta.removed
    );
    assert!(
        delta.changed.len() < after.entities.len() / 2,
        "{} of {} records changed; a delta that carries the static world is a keyframe",
        delta.changed.len(),
        after.entities.len()
    );
    let rebuilt =
        apply_delta(&registry, &before, &delta).expect("the registry can split every record");
    assert_eq!(rebuilt.entities, after.entities);
    assert_eq!(rebuilt.resources, after.resources);
}

/// Fifty static entities: the keyframe carries them all, the delta after it carries none.
#[test]
fn a_delta_is_a_fraction_of_the_keyframe_it_follows() {
    let mut net = session(1, 50);
    let (host, client) = (net.host(), net.client());
    net.trace_packets();
    net.run(200);
    let packets = net.decode_snapshots(host, client);
    let sizes = net.snapshots_sent(host, client);
    assert_eq!(packets.len(), sizes.len(), "every traced snapshot decodes");
    let mut pairs = 0;
    for window in packets.iter().zip(&sizes).collect::<Vec<_>>().windows(2) {
        let ((first, first_size), (second, second_size)) = (window[0], window[1]);
        if let (SnapshotBody::Full(_), SnapshotBody::Delta(_)) = (&first.body, &second.body) {
            pairs += 1;
            assert!(
                second_size.bytes * 10 <= first_size.bytes,
                "the delta after a {}-byte keyframe is {} bytes, more than a tenth of it",
                first_size.bytes,
                second_size.bytes
            );
        }
    }
    assert!(pairs >= 1, "no keyframe followed by a delta in 200 frames");
}

/// A once-only component travels in the records that introduce the entity — one per delta
/// built before the client's acknowledgement of the first came back — and never again. Its
/// neighbour that moves every tick rides every delta, so the record was compared, not skipped.
#[test]
fn a_once_component_is_sent_exactly_once_per_entity() {
    let mut net = session(1, 4);
    let (host, client) = (net.host(), net.client());
    net.trace_packets();
    net.run(4);
    let newcomer = net
        .world_mut(host)
        .spawn_tracked((Pos(5), Vel(1), EntityKind(4)));
    let id = net
        .app(host)
        .world()
        .get::<bevy_ticked::tracked_entity::TickTrackedEntity>(newcomer)
        .unwrap()
        .0;
    net.run(200);
    let registry = net
        .app(host)
        .world()
        .resource::<TickedComponentRegistry>()
        .clone();
    let kind = registry.wire_index_of::<EntityKind>().unwrap();
    let pos = registry.wire_index_of::<Pos>().unwrap();
    let (mut kinds, mut positions, mut deltas) = (Vec::new(), 0, 0);
    for packet in net.decode_snapshots(host, client) {
        let SnapshotBody::Delta(delta) = &packet.body else {
            continue;
        };
        deltas += 1;
        for record in delta.changed.iter().filter(|record| record.id == id) {
            if record.present.contains(kind) {
                kinds.push(packet.seq);
            }
            positions += usize::from(record.present.contains(pos));
        }
    }
    assert!(deltas > 100, "{deltas} deltas in 200 frames");
    let first = *kinds
        .first()
        .expect("the newcomer was introduced in a delta");
    let round_trip_packets = 4;
    assert!(
        kinds.iter().all(|seq| *seq < first + round_trip_packets),
        "`EntityKind` rode deltas {kinds:?}: once per baseline that predates the entity, then never"
    );
    assert!(
        positions > 100,
        "the moving position rode {positions} deltas, so the record was compared"
    );
    assert_eq!(
        latest::<EntityKind>(net.app(client), id),
        Some(EntityKind(4))
    );
}

/// A component removed on the host and an entity that died there both reach the client
/// through deltas, and the client's world shows neither.
#[test]
fn a_removed_component_and_a_despawned_entity_travel_in_a_delta() {
    let mut net = session(1, 6);
    let (host, client) = (net.host(), net.client());
    net.trace_packets();
    let ids = tracked_ids(net.app_mut(host));
    let (stripped_id, dead_id) = (ids[ids.len() - 2], ids[ids.len() - 1]);
    let stripped = tracked_entity(net.app(host), stripped_id).unwrap();
    let dead = tracked_entity(net.app(host), dead_id).unwrap();
    net.world_mut(host).entity_mut(stripped).remove::<Vel>();
    net.world_mut(host).entity_mut(dead).despawn_ticked();
    net.run(64);

    let (mut removed_seen, mut despawned_seen) = (false, false);
    for packet in net.decode_snapshots(host, client) {
        if let SnapshotBody::Delta(delta) = &packet.body {
            removed_seen |= delta.removed.iter().any(|(id, _)| *id == stripped_id);
            despawned_seen |= delta.despawned.contains(&dead_id);
        }
    }
    assert!(removed_seen, "no delta carried the removal");
    assert!(despawned_seen, "no delta carried the despawn");
    assert!(
        latest::<Vel>(net.app(client), stripped_id).is_none(),
        "the client still has the removed component"
    );
    assert!(
        tracked_entity(net.app(client), dead_id).is_none(),
        "the client still has the dead entity"
    );
}

// ── Acks and baselines ───────────────────────────────────────────────────────

#[test]
fn an_ack_rides_the_input_packet() {
    let mut net = session(1, 0);
    let (host, client) = (net.host(), net.client());
    net.trace_packets();
    net.run(32);
    let acks: Vec<u32> = net
        .decode_messages::<EnsembleInputMessage<Input>>(client, host)
        .into_iter()
        .filter_map(|message| message.payload.ack)
        .collect();
    assert!(
        acks.len() >= 30,
        "{} input packets carried an ack",
        acks.len()
    );
    assert!(
        acks.windows(2).all(|w| w[1] >= w[0]),
        "acks go backwards: {acks:?}"
    );
    assert!(
        acks.last() > acks.first(),
        "the ack never advanced: {acks:?}"
    );
}

/// Every delta names a baseline the client acknowledged, on an input packet that had reached
/// the host, before the delta was built.
#[test]
fn a_delta_is_never_built_against_a_baseline_the_client_did_not_ack() {
    // Traced from the first packet: an acknowledgement sent before the trace began would look
    // like one never sent.
    let mut net = TickedNetwork::client_server::<Input>(1, minimal::install)
        .with_link(Link::bad_wifi())
        .with_seed(21);
    net.trace_packets();
    let (host, client) = (net.host(), net.client());
    for i in 0..10 {
        net.world_mut(host)
            .spawn_tracked((Pos(100 + i), Vel(0), EntityKind(9)));
    }
    assert!(net.settle(SETTLE));
    seat_everyone(&mut net);
    net.run(600);
    // Frames at which each ack reached the host.
    let acked: Vec<(u64, u32)> = net
        .decode_messages_traced::<EnsembleInputMessage<Input>>(client, host)
        .into_iter()
        .filter_map(|traced| Some((traced.arrived_frame?, traced.message.payload.ack?)))
        .collect();
    let mut deltas = 0;
    for traced in net
        .decode_messages_traced::<bevy_ticked_networking_ensemble::EnsembleSnapshotMessage>(
            host, client,
        )
    {
        let Some(packet) = bevy_ticked_networking::snapshot::decode_packet(&traced.message.bytes)
        else {
            continue;
        };
        let SnapshotBody::Delta(delta) = &packet.body else {
            continue;
        };
        deltas += 1;
        assert!(
            acked
                .iter()
                .any(|(at, ack)| *at <= traced.sent_frame && *ack == delta.baseline_seq),
            "a delta sent on frame {} was built against seq {}, which the client had not acknowledged",
            traced.sent_frame,
            delta.baseline_seq
        );
    }
    assert!(deltas > 100, "{deltas} deltas over 600 frames of bad wifi");
}

/// The host keeps building against the last acknowledged baseline, so losing deltas costs
/// nothing but the ticks they carried.
#[test]
fn a_lost_delta_falls_back_to_the_last_acked_baseline() {
    let mut net = session(1, 10);
    let (host, client) = (net.host(), net.client());
    net.hold_input(client, Input::RIGHT, 8);
    let applied_before = replays(net.app(client)).snapshots_applied;
    drop_next_packets(&mut net, host, client, 4);
    net.hold_input(client, Input::RIGHT, 32);
    let stats = replays(net.app(client));
    assert_eq!(
        stats.dropped_unknown_baseline, 0,
        "a baseline the client acked was gone: {stats:?}"
    );
    assert!(
        stats.snapshots_applied >= applied_before + 20,
        "the stream resumed: {stats:?}"
    );
    for id in static_ids(&mut net) {
        assert_converged::<Pos>(&net, id, |a, b| (a.0 - b.0).abs() as f32, 0.0);
    }
}

/// When the client's acks stop arriving, the acknowledged baseline falls off the host's ring
/// and the next packet is a keyframe, without waiting for the periodic one.
#[test]
fn a_keyframe_is_sent_after_n_consecutive_losses() {
    let mut net = session_with(1, 10, 3, Link::cable(), |app| {
        app.insert_resource(DeltaPolicy {
            keyframe_every: 1_000_000,
            max_unacked_baselines: 8,
            enabled: true,
        });
    });
    let (host, client) = (net.host(), net.client());
    net.trace_packets();
    net.run(16);
    let before = deltas_of(&net, host, client);
    assert!(before >= 10, "{before} deltas before the loss");
    // Every packet the client sends for a while is lost: no ack reaches the host.
    net.net
        .set_link_between(client, host, Link::cable().with_loss(1.0));
    net.run(24);
    net.net.set_link_between(client, host, Link::cable());
    let keyframes = net
        .decode_snapshots(host, client)
        .iter()
        .skip(1)
        .filter(|packet| matches!(packet.body, SnapshotBody::Full(_)))
        .count();
    assert!(
        keyframes >= 1,
        "no keyframe once the acked baseline was evicted"
    );
    net.run(16);
    let stats = snapshot_stats(net.app(host));
    assert!(
        stats.deltas > before as u64,
        "deltas resumed once acks did: {stats:?}"
    );
}

/// A client cannot rebuild a delta whose baseline it never held: it asks, and the host's next
/// packet is a full body.
#[test]
fn a_nacked_client_gets_a_full_body_next() {
    let mut net = session(1, 5);
    let (host, client) = (net.host(), net.client());
    net.trace_packets();
    let before = snapshot_stats(net.app(host)).keyframes;
    // Forget every body the client holds: the next delta has no baseline.
    net.world_mut(client)
        .resource_mut::<bevy_ticked_networking::replication::AuthoritativeHistory>()
        .clear();
    net.run(8);
    let stats = replays(net.app(client));
    assert!(stats.dropped_unknown_baseline >= 1, "{stats:?}");
    let after = snapshot_stats(net.app(host)).keyframes;
    assert!(
        after > before,
        "the host never answered the nack with a full body"
    );
    assert!(
        applied_tick(net.app(client))
            .is_some_and(|t| t + 8 >= bevy_ticked_testing::view::tick(net.app(host))),
        "the client is back on the stream"
    );
}

#[test]
fn late_join_still_gets_a_full_world() {
    let mut net = session(1, 12);
    let host = net.host();
    net.run(100);
    net.trace_packets();
    let joiner = net.add_client();
    assert!(net.settle(SETTLE));
    let first = net
        .decode_snapshots(host, joiner)
        .into_iter()
        .next()
        .expect("the joiner was sent a snapshot");
    let SnapshotBody::Full(body) = first.body else {
        panic!("the joiner's first packet was a delta against nothing");
    };
    assert_eq!(body.entities.len(), tracked_entity_count(net.app(host)));
    assert_eq!(
        tracked_entity_count(net.app(joiner)),
        tracked_entity_count(net.app(host))
    );
}

/// Ten thousand ticks of bad wifi. Whatever was lost, the client is never left applying
/// nothing because every delta names a baseline it dropped.
#[test]
fn a_lossy_link_never_leaves_a_client_on_a_stale_baseline() {
    let mut net = session_with(2, 8, 77, Link::bad_wifi(), |app| {
        app.add_systems(PreUpdate, walk_right);
    });
    let (host, clients) = (net.host(), net.clients());
    let mut worst_gap = 0u64;
    for _ in 0..100 {
        net.run(100);
        for client in &clients {
            let applied = applied_tick(net.app(*client)).unwrap_or(0);
            let host_tick = bevy_ticked_testing::view::tick(net.app(host));
            worst_gap = worst_gap.max(host_tick.saturating_sub(applied));
        }
    }
    assert!(
        worst_gap < 64,
        "a client fell {worst_gap} ticks behind the host's snapshots"
    );
    for client in &clients {
        let stats = replays(net.app(*client));
        assert!(
            stats.deltas_applied > stats.dropped_unknown_baseline * 10,
            "{stats:?}: most deltas must rebuild, or the link is all keyframes"
        );
    }
    for id in static_ids(&mut net) {
        assert_converged::<Pos>(&net, id, |a, b| (a.0 - b.0).abs() as f32, 0.0);
    }
}

// ── Compression and the numbers ──────────────────────────────────────────────

#[test]
fn lz4_makes_the_stable_stream_smaller_not_larger() {
    fn bytes_with(compression: Compression) -> (u64, usize) {
        let mut net = session_with(
            1,
            60,
            5,
            Link::cable(),
            match compression {
                Compression::None => |app: &mut App| {
                    app.insert_resource(SnapshotCompression(Compression::None));
                },
                Compression::Lz4 => |app: &mut App| {
                    app.insert_resource(SnapshotCompression(Compression::Lz4));
                },
            },
        );
        let (host, client) = (net.host(), net.client());
        let before = net.bytes_sent(host, client);
        let report = measure_snapshot_size(&mut net, host, client, 130);
        (net.bytes_sent(host, client) - before, report.max_bytes)
    }
    let (raw, raw_max) = bytes_with(Compression::None);
    let (lz4, lz4_max) = bytes_with(Compression::Lz4);
    println!(
        "130 frames, 60 static entities: {raw} bytes raw ({raw_max} max), {lz4} bytes lz4 ({lz4_max} max)"
    );
    assert!(lz4 <= raw, "lz4 cost bytes: {lz4} > {raw}");
    assert!(
        lz4_max < raw_max,
        "the keyframe did not shrink: {lz4_max} vs {raw_max}"
    );
}

fn walk_right(
    local: Option<Res<bevy_ticked_networking::client::LocalClientPlayer>>,
    tick: Res<bevy_ticked::tick::CurrentTick>,
    mut queue: ResMut<bevy_ticked_networking::input::InputQueue<Input>>,
) {
    if let Some(local) = local {
        queue.insert(tick.0 + 1, local.0, Input::RIGHT);
    }
}

#[test]
fn bytes_per_tick_at_rest_is_under_40() {
    let mut net = session(1, 0);
    let (host, client) = (net.host(), net.client());
    net.run(64);
    let per_tick = assert_bandwidth_within(&mut net, host, client, 256, 40);
    println!("host -> client at rest: {per_tick:.1} bytes per tick");
}

#[test]
fn bytes_per_tick_walking_is_under_200() {
    let mut net = session_with(2, 0, 12, Link::cable(), |app| {
        app.add_systems(PreUpdate, walk_right);
    });
    let (host, clients) = (net.host(), net.clients());
    net.run(128);
    for client in clients {
        let per_tick = assert_bandwidth_within(&mut net, host, client, 256, 200);
        println!(
            "host -> client {client:?}: {per_tick:.1} bytes per tick for two walking players (audit: 1340)"
        );
    }
}
