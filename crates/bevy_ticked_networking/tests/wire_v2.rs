//! The snapshot wire, version 2: entity-major, sorted, addressed.

use std::cell::Cell;
use std::time::Duration;

use bevy::prelude::*;
use bevy::time::TimeUpdateStrategy;
use bevy_ticked::prelude::*;
use bevy_ticked::registry::TickedComponentRegistry;
use bevy_ticked::tracked_entity::TickTrackedEntity;
use bevy_ticked_networking::client::{LastAppliedSeq, LocalClientPlayer};
use bevy_ticked_networking::diagnostics::SNAPSHOT_ADVISORY_BYTES;
use bevy_ticked_networking::input::InputQueue;
use bevy_ticked_networking::messages::{ReceivedNetworkSnapshot, SendNetworkInput, SendNetworkSnapshot};
use bevy_ticked_networking::prelude::*;
use bevy_ticked_networking::server::{InputMargins, LocalServerPlayer, SnapshotRecipientList};
use bevy_ticked_networking::snapshot::{
    DeltaBody, EntityRecord, FullBody, RelayedInput, SnapshotBody, SnapshotPacket, apply_full_body,
    build_full_body, decode_packet, encode_packet,
};
use serde::{Deserialize, Deserializer, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
struct Input {
    dx: i8,
}

#[derive(Component, Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
struct Pos(i32);

#[derive(Component, Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
struct Vel(i32);

#[derive(Component, Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
struct Kind(u8);

/// A component that counts its own decodes.
#[derive(Component, Clone, Copy, Debug, PartialEq, Serialize)]
struct Counted(u32);

thread_local! {
    /// Per test thread: `apply_full_body` decodes on the calling thread, and the tests run in
    /// parallel.
    static DECODES: Cell<usize> = const { Cell::new(0) };
}

impl<'de> Deserialize<'de> for Counted {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        DECODES.with(|count| count.set(count.get() + 1));
        u32::deserialize(deserializer).map(Counted)
    }
}

#[derive(Resource, Clone, Default, Debug, PartialEq, Serialize, Deserialize)]
struct Round(u32);

const TICK: Duration = Duration::from_micros(15_625);

fn peer() -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .add_plugins(TickedPlugin {
            source: TickSource::Hz(64.0),
            ..default()
        })
        .add_plugins(TickedServerPlugin::<Input>::new())
        .add_plugins(TickedClientPlugin::<Input>::new())
        .insert_resource(TimeUpdateStrategy::ManualDuration(TICK))
        // Registered in an order that is not the sorted one, on purpose.
        .register_networked_ticked_component::<Vel>("Vel")
        .register_networked_ticked_component::<Pos>("Pos")
        .register_networked_ticked_component::<Kind>("Kind")
        .register_networked_ticked_component::<Counted>("Counted")
        .register_networked_ticked_resource::<Round>("Round");
    app
}

fn capture(app: &mut App, tick: u64) {
    let registry = app.world().resource::<TickedComponentRegistry>().clone();
    registry.capture_all(app.world_mut(), tick);
}

fn world_with_bodies(n: u64) -> App {
    let mut app = peer();
    for id in 1..=n {
        app.world_mut().spawn((
            TickTrackedEntity(id),
            Pos(id as i32 * 10),
            Vel(-(id as i32)),
            Kind(1),
            Counted(id as u32),
        ));
    }
    app.insert_resource(Round(7));
    capture(&mut app, 1);
    app
}

fn components(app: &mut App) -> Vec<(u64, Pos, Vel, Kind)> {
    let mut q = app.world_mut().query::<(&TickTrackedEntity, &Pos, &Vel, &Kind)>();
    let mut all: Vec<_> = q
        .iter(app.world())
        .map(|(t, p, v, k)| (t.0, *p, *v, *k))
        .collect();
    all.sort_by_key(|(id, ..)| *id);
    all
}

// ── shape ────────────────────────────────────────────────────────────────────

#[test]
fn the_same_world_encodes_to_the_same_bytes_twice() {
    let mut app = world_with_bodies(5);
    let a = encode_packet(&SnapshotPacket::full(1, build_full_body(app.world_mut(), 1)));
    let b = encode_packet(&SnapshotPacket::full(1, build_full_body(app.world_mut(), 1)));
    assert_eq!(a, b, "a delta needs a baseline it can reproduce byte for byte");
}

#[test]
fn a_snapshot_round_trips() {
    let mut host = world_with_bodies(4);
    let mut client = peer();
    let body = build_full_body(host.world_mut(), 1);
    let packet = SnapshotPacket::full(1, body);
    let decoded = decode_packet(&encode_packet(&packet)).expect("decodes");
    assert_eq!(decoded, packet);

    let applied = apply_full_body(client.world_mut(), 1, decoded.full_body().unwrap());
    assert_eq!(applied.spawned.len(), 4);
    assert!(applied.undecodable.is_empty());
    assert_eq!(components(&mut host), components(&mut client));
    assert_eq!(client.world().resource::<Round>(), &Round(7));
    assert_eq!(client.world().resource::<CurrentTick>().0, 1);
}

#[test]
fn records_are_sorted_and_indices_follow_the_names() {
    let mut app = world_with_bodies(3);
    let registry = app.world().resource::<TickedComponentRegistry>().clone();
    let body = build_full_body(app.world_mut(), 1);
    let ids: Vec<u64> = body.ids().collect();
    assert_eq!(ids, vec![1, 2, 3]);
    // "Counted" < "Kind" < "Pos" < "Vel"
    assert_eq!(registry.wire_index_of::<Counted>(), Some(0));
    assert_eq!(registry.wire_index_of::<Kind>(), Some(1));
    assert_eq!(registry.wire_index_of::<Pos>(), Some(2));
    assert_eq!(registry.wire_index_of::<Vel>(), Some(3));
    let record = body.record(2).unwrap();
    assert_eq!(record.present.iter().collect::<Vec<_>>(), vec![0, 1, 2, 3]);
    assert_eq!(record.first::<Counted>(), Some(Counted(2)), "the first type is Counted");
}

#[test]
fn entity_major_encoding_has_no_length_prefixes() {
    let mut app = world_with_bodies(1);
    let body = build_full_body(app.world_mut(), 1);
    let record = body.record(1).unwrap();
    let expected = postcard::to_allocvec(&Counted(1)).unwrap().len()
        + postcard::to_allocvec(&Kind(1)).unwrap().len()
        + postcard::to_allocvec(&Pos(10)).unwrap().len()
        + postcard::to_allocvec(&Vel(-1)).unwrap().len();
    assert_eq!(
        record.bytes.len(),
        expected,
        "the components' own encodings, concatenated, and nothing else"
    );
}

#[test]
fn apply_snapshot_decodes_each_component_once() {
    let mut host = world_with_bodies(6);
    let mut client = peer();
    let body = build_full_body(host.world_mut(), 1);
    DECODES.with(|count| count.set(0));
    apply_full_body(client.world_mut(), 1, &body);
    assert_eq!(
        DECODES.with(|count| count.get()),
        6,
        "six records, six decodes of Counted: the body is walked once, not once per type"
    );
}

#[test]
fn a_duplicate_id_in_a_body_is_applied_once_and_reported() {
    let mut client = peer();
    let registry = client.world().resource::<TickedComponentRegistry>().clone();
    let pos = registry.wire_index_of::<Pos>().unwrap();
    let mut body = FullBody::default();
    body.entities.push(EntityRecord::new(1).with(pos, &Pos(1)));
    body.entities.push(EntityRecord::new(1).with(pos, &Pos(2)));
    let applied = apply_full_body(client.world_mut(), 1, &body);
    assert_eq!(applied.duplicate_ids, vec![1]);
    let mut q = client.world_mut().query::<&Pos>();
    assert_eq!(q.iter(client.world()).copied().collect::<Vec<_>>(), vec![Pos(1)]);
}

#[test]
fn a_record_with_garbage_bytes_is_reported_not_panicked() {
    let mut client = peer();
    let registry = client.world().resource::<TickedComponentRegistry>().clone();
    let pos = registry.wire_index_of::<Pos>().unwrap();
    let mut record = EntityRecord::new(9);
    record.present.set(pos);
    record.bytes = vec![0xFF; 1]; // a truncated varint
    let body = FullBody {
        entities: vec![record],
        ..Default::default()
    };
    let applied = apply_full_body(client.world_mut(), 1, &body);
    assert_eq!(applied.undecodable, vec![(9, pos)]);
}

// ── the client and the packet ────────────────────────────────────────────────

fn client_app() -> App {
    let mut app = peer();
    app.insert_resource(LocalClientPlayer(7));
    app.update();
    app
}

#[test]
fn a_delta_body_is_rejected_until_the_delta_phase() {
    let mut app = client_app();
    let packet = SnapshotPacket {
        seq: 1,
        tick: 3,
        your_margin: 2,
        body: SnapshotBody::Delta(DeltaBody::default()),
    };
    app.world_mut().trigger(ReceivedNetworkSnapshot(packet));
    app.update();
    let stats = replays_of(&app);
    assert_eq!(stats.dropped_delta_body, 1);
    assert_eq!(stats.snapshots_applied, 0);
    assert!(
        app.world().resource::<TickHolds>().holds(TickHoldReason::AwaitingSync),
        "still waiting for a world it can apply"
    );
}

fn replays_of(app: &App) -> bevy_ticked_networking::diagnostics::ReplayStats {
    *app.world().resource::<bevy_ticked_networking::diagnostics::ReplayStats>()
}

#[test]
fn relayed_inputs_reach_the_clients_queue_and_its_own_are_ignored() {
    let mut app = client_app();
    let mut body = FullBody::default();
    let registry = app.world().resource::<TickedComponentRegistry>().clone();
    let pos = registry.wire_index_of::<Pos>().unwrap();
    body.put(EntityRecord::new(1).with(pos, &Pos(0)));
    body.inputs_ahead = vec![
        RelayedInput {
            player: 9,
            tick: 5,
            bytes: postcard::to_allocvec(&Input { dx: 1 }).unwrap(),
        },
        RelayedInput {
            player: 7,
            tick: 5,
            bytes: postcard::to_allocvec(&Input { dx: -1 }).unwrap(),
        },
    ];
    app.world_mut()
        .trigger(ReceivedNetworkSnapshot(SnapshotPacket::full(2, body)));
    app.update();

    let queue = app.world().resource::<InputQueue<Input>>();
    assert_eq!(queue.get(5, 9), Some(&Input { dx: 1 }), "another player's input arrived");
    assert_eq!(queue.get(5, 7), None, "the local player's own is not overwritten by a relay");
}

#[test]
fn a_client_acks_the_newest_seq_on_its_input() {
    #[derive(Resource, Default)]
    struct Acks(Vec<Option<u32>>);
    let mut app = client_app();
    app.init_resource::<Acks>().add_observer(
        |trigger: On<SendNetworkInput<Input>>, mut acks: ResMut<Acks>| {
            acks.0.push(trigger.event().ack);
        },
    );
    let registry = app.world().resource::<TickedComponentRegistry>().clone();
    let pos = registry.wire_index_of::<Pos>().unwrap();
    let mut body = FullBody::default();
    body.put(EntityRecord::new(1).with(pos, &Pos(0)));
    let mut packet = SnapshotPacket::full(0, body);
    packet.seq = 41;
    app.world_mut().trigger(ReceivedNetworkSnapshot(packet));
    app.update();
    assert_eq!(app.world().resource::<LastAppliedSeq>().0, Some(41));

    for _ in 0..3 {
        let tick = app.world().resource::<CurrentTick>().0;
        app.world_mut()
            .resource_mut::<InputQueue<Input>>()
            .insert(tick + 1, 7, Input { dx: 1 });
        app.update();
    }
    let acks = &app.world().resource::<Acks>().0;
    assert!(!acks.is_empty(), "input was sent");
    assert!(acks.iter().all(|ack| *ack == Some(41)), "every packet carries the ack: {acks:?}");
}

// ── the server and its recipients ────────────────────────────────────────────

#[derive(Resource, Default)]
struct Sent(Vec<(Option<u128>, SnapshotPacket)>);

fn host_app() -> App {
    let mut app = world_with_bodies(2);
    app.init_resource::<Sent>().add_observer(
        |trigger: On<SendNetworkSnapshot>, mut sent: ResMut<Sent>| {
            let event = trigger.event();
            sent.0
                .push((event.recipient, decode_packet(&event.bytes).expect("decodes")));
        },
    );
    app.insert_resource(LocalServerPlayer(1));
    app.update();
    app
}

#[test]
fn each_packet_carries_the_recipients_seq_and_margin() {
    let mut app = host_app();
    app.insert_resource(SnapshotRecipientList(vec![2, 3]));
    app.world_mut()
        .resource_mut::<InputMargins>()
        .0
        .extend([(2u128, 5i64), (3u128, -3i64)]);
    for _ in 0..3 {
        app.update();
    }
    let sent = &app.world().resource::<Sent>().0;
    let to = |uuid: u128| -> Vec<&SnapshotPacket> {
        sent.iter()
            .filter(|(r, _)| *r == Some(uuid))
            .map(|(_, p)| p)
            .collect()
    };
    let (two, three) = (to(2), to(3));
    assert!(two.len() >= 3 && three.len() >= 3, "one packet per recipient per tick");
    assert!(two.iter().all(|p| p.your_margin == 5));
    assert!(three.iter().all(|p| p.your_margin == -3));
    let seqs: Vec<u32> = two.iter().map(|p| p.seq).collect();
    assert!(
        seqs.windows(2).all(|w| w[1] == w[0] + 1),
        "sequence numbers count per recipient: {seqs:?}"
    );
    assert_eq!(two[0].tick, three[0].tick, "the same tick goes to everyone");
    assert!(
        sent.iter().all(|(r, _)| r.is_some()),
        "nothing unaddressed when the list is present"
    );
}

#[test]
fn an_absent_recipient_list_sends_one_unaddressed_packet() {
    let mut app = host_app();
    app.update();
    let sent = &app.world().resource::<Sent>().0;
    assert!(!sent.is_empty());
    assert!(sent.iter().all(|(r, p)| r.is_none() && p.seq == 0));
}

#[test]
fn an_oversize_snapshot_is_counted_and_warned_once() {
    let mut app = peer();
    // Enough bodies to pass the advisory size several times over.
    for id in 1..=400u64 {
        app.world_mut()
            .spawn((TickTrackedEntity(id), Pos(id as i32), Vel(1), Kind(2), Counted(3)));
    }
    app.insert_resource(LocalServerPlayer(1));
    app.update();
    for _ in 0..4 {
        app.update();
    }
    let stats = *app
        .world()
        .resource::<bevy_ticked_networking::diagnostics::SnapshotStats>();
    assert!(stats.last_bytes > SNAPSHOT_ADVISORY_BYTES);
    assert!(stats.oversize >= 4, "every oversize packet is counted: {stats:?}");
}
