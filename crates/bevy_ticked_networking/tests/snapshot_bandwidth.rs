//! What the snapshot costs, and what the knobs that shrink it give up in exchange.
//!
//! `coop_zombies/docs/upstream-needs.md` T1 and T2. Both knobs default to off, so the first thing
//! these assert is that the defaults changed nothing; the rest measure what turning them on buys
//! and — the part worth writing down — what it breaks if it is turned on carelessly.

use std::collections::HashMap;

use bevy::prelude::*;
use bevy_ticked::lifetimes::TrackedEntityLifetimes;
use bevy_ticked::prelude::TickedAppExt;
use bevy_ticked::registry::TickedComponentRegistry;
use bevy_ticked::tick::CurrentTick;
use bevy_ticked::tracked_entity::{TickTrackedEntity, TickTrackedEntityCounter};
use bevy_ticked_networking::prelude::*;
use bevy_ticked_networking::snapshot::{
    apply_snapshot, apply_snapshot_with, build_snapshot, SnapshotBaseline, SnapshotSendRates,
    WorldSnapshot,
};
use serde::{Deserialize, Serialize};

#[derive(Component, Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
struct Pos(i32);

/// The shape T1 is really about: a fact that changes once a round and is re-sent every tick.
#[derive(Component, Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
struct Round(u32);

fn peer() -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .init_resource::<TickedComponentRegistry>()
        .init_resource::<TrackedEntityLifetimes>()
        .init_resource::<CurrentTick>()
        .init_resource::<TickTrackedEntityCounter>()
        .register_networked_ticked_component_as::<Pos>("Pos")
        .register_networked_ticked_component_as::<Round>("Round");
    app
}

fn capture(app: &mut App, tick: u64) {
    let registry = app.world().resource::<TickedComponentRegistry>().clone();
    registry.capture_all(app.world_mut(), tick);
}

fn full(app: &mut App, tick: u64) -> WorldSnapshot {
    capture(app, tick);
    build_snapshot(app.world_mut(), tick)
}

fn wire_size(snapshot: &WorldSnapshot) -> usize {
    postcard::to_allocvec(snapshot).unwrap().len()
}

fn positions(app: &mut App) -> Vec<(u64, i32)> {
    let mut q = app.world_mut().query::<(&TickTrackedEntity, &Pos)>();
    let mut seen: Vec<(u64, i32)> = q
        .iter(app.world())
        .map(|(tracked, pos)| (tracked.0, pos.0))
        .collect();
    seen.sort();
    seen
}

// ── the defaults ─────────────────────────────────────────────────────────────

/// A snapshot built the ordinary way is a keyframe, and a keyframe is what every peer already
/// understood. Turning nothing on must change nothing.
#[test]
fn a_snapshot_is_a_keyframe_unless_asked_otherwise() {
    let mut host = peer();
    host.world_mut().spawn((TickTrackedEntity(1), Pos(3)));
    let snapshot = full(&mut host, 1);

    assert!(snapshot.keyframe, "the default snapshot carries everything");
    assert!(snapshot.removed.is_empty(), "removal travels by absence on a keyframe");
    assert_eq!(snapshot.entities, vec![1], "and it says who exists");
}

// ── T1: change detection ─────────────────────────────────────────────────────

/// The measurement. A world where one entity moves and a round counter does not: the delta
/// carries the mover and drops the rest.
#[test]
fn a_delta_carries_only_what_changed() {
    let mut host = peer();
    let mover = host.world_mut().spawn((TickTrackedEntity(1), Pos(0))).id();
    host.world_mut().spawn((TickTrackedEntity(2), Pos(50)));
    host.world_mut().spawn((TickTrackedEntity(3), Round(7)));

    let mut baseline = SnapshotBaseline::default();
    let rates = SnapshotSendRates::default();

    let first = full(&mut host, 1);
    baseline.prime(&first);

    host.world_mut().entity_mut(mover).insert(Pos(1));
    let second = full(&mut host, 2);
    let delta = baseline.reduce(&second, &rates);

    let changed: Vec<u64> = delta
        .components
        .values()
        .flat_map(|entities| entities.keys().copied())
        .collect();
    assert_eq!(changed, vec![1], "only the entity that moved is on the wire");
    assert!(
        wire_size(&delta) < wire_size(&second),
        "and the delta is smaller than the snapshot it replaces: {} vs {}",
        wire_size(&delta),
        wire_size(&second)
    );
    assert_eq!(
        delta.entities,
        vec![1, 2, 3],
        "existence still travels in full — it is what stops a delta reading as three despawns"
    );
}

/// The property the whole design turns on: a client folding deltas into a baseline ends up with
/// exactly the world it would have had from keyframes.
#[test]
fn a_stream_of_deltas_reconstructs_the_same_world_as_a_stream_of_keyframes() {
    let mut host = peer();
    let a = host.world_mut().spawn((TickTrackedEntity(1), Pos(0))).id();
    let b = host.world_mut().spawn((TickTrackedEntity(2), Pos(100))).id();

    let mut from_keyframes = peer();
    let mut from_deltas = peer();
    let mut send = SnapshotBaseline::default();
    let mut receive = SnapshotBaseline::default();
    let rates = SnapshotSendRates::default();

    for tick in 1..=8u64 {
        // A moves every tick; B moves only twice; nobody touches Round at all.
        host.world_mut().entity_mut(a).insert(Pos(tick as i32));
        if tick == 3 || tick == 6 {
            host.world_mut().entity_mut(b).insert(Pos(100 + tick as i32));
        }

        let keyframe = full(&mut host, tick);
        apply_snapshot(from_keyframes.world_mut(), &keyframe);

        // Every fourth send is a keyframe, the rest are deltas — the shape `keyframe_every = 4`
        // produces.
        let on_the_wire = if tick % 4 == 1 {
            send.prime(&keyframe);
            keyframe.clone()
        } else {
            send.reduce(&keyframe, &rates)
        };
        let components = receive.absorb(&on_the_wire);
        apply_snapshot_with(from_deltas.world_mut(), &on_the_wire, &components);

        assert_eq!(
            positions(&mut from_deltas),
            positions(&mut from_keyframes),
            "the two clients disagree at tick {tick}"
        );
    }
}

/// A removal has to travel explicitly on a delta, because absence has been spent on "unchanged".
#[test]
fn a_delta_says_a_component_was_removed_rather_than_leaving_it_out() {
    let mut host = peer();
    let entity = host
        .world_mut()
        .spawn((TickTrackedEntity(1), Pos(0), Round(2)))
        .id();

    let mut send = SnapshotBaseline::default();
    let mut receive = SnapshotBaseline::default();
    let mut client = peer();
    let rates = SnapshotSendRates::default();

    let first = full(&mut host, 1);
    send.prime(&first);
    let components = receive.absorb(&first);
    apply_snapshot_with(client.world_mut(), &first, &components);
    let on_client = entity_of(&mut client, 1);
    assert!(client.world().entity(on_client).get::<Round>().is_some());

    host.world_mut().entity_mut(entity).remove::<Round>();
    let second = full(&mut host, 2);
    let delta = send.reduce(&second, &rates);

    assert!(
        !delta.removed.is_empty(),
        "the removal has to be stated; absence now means unchanged"
    );
    let components = receive.absorb(&delta);
    apply_snapshot_with(client.world_mut(), &delta, &components);
    let on_client = entity_of(&mut client, 1);
    assert!(
        client.world().entity(on_client).get::<Round>().is_none(),
        "and it lands"
    );
}

/// A dead entity costs one absence from `entities`, not one removal per component it carried.
#[test]
fn a_dead_entity_does_not_emit_a_removal_per_component() {
    let mut host = peer();
    host.world_mut().spawn((TickTrackedEntity(1), Pos(0)));
    let doomed = host
        .world_mut()
        .spawn((TickTrackedEntity(2), Pos(9), Round(1)))
        .id();

    let mut send = SnapshotBaseline::default();
    let rates = SnapshotSendRates::default();
    let first = full(&mut host, 1);
    send.prime(&first);

    host.world_mut().despawn(doomed);
    let second = full(&mut host, 2);
    let delta = send.reduce(&second, &rates);

    assert_eq!(delta.entities, vec![1], "existence carries the death");
    assert!(
        delta.removed.is_empty(),
        "and nothing else has to: got {:?}",
        delta.removed
    );
}

// ── T1: per-type send rate ───────────────────────────────────────────────────

/// A type held to every fourth tick is simply not looked at in between, so its change waits.
#[test]
fn a_type_with_a_send_rate_is_held_back_between_its_ticks() {
    let mut host = peer();
    let entity = host.world_mut().spawn((TickTrackedEntity(1), Round(1))).id();

    let round_index = host
        .world()
        .resource::<TickedComponentRegistry>()
        .index_of::<Round>()
        .unwrap();
    let mut rates = SnapshotSendRates::default();
    rates.set(round_index, 4);

    let mut send = SnapshotBaseline::default();
    send.prime(&full(&mut host, 0));

    host.world_mut().entity_mut(entity).insert(Round(2));

    // Ticks 1..3 are not multiples of 4, so Round is not even compared.
    for tick in 1..4u64 {
        let delta = send.reduce(&full(&mut host, tick), &rates);
        assert!(
            !delta.components.contains_key(&round_index),
            "Round should be held back at tick {tick}"
        );
    }
    let delta = send.reduce(&full(&mut host, 4), &rates);
    assert!(
        delta.components.contains_key(&round_index),
        "and go out on the tick it is due"
    );
}

// ── the trade, stated ────────────────────────────────────────────────────────

/// A receiver that has never seen a keyframe cannot decode a delta, and must not try.
///
/// This is the joiner's window, and the reason `ForceKeyframe` exists: without it a client that
/// arrives mid-interval would apply a delta against an empty baseline and build a world made of
/// whatever happened to have changed that tick.
#[test]
fn a_delta_is_meaningless_before_a_keyframe() {
    let mut host = peer();
    host.world_mut().spawn((TickTrackedEntity(1), Pos(5)));

    let mut send = SnapshotBaseline::default();
    send.prime(&full(&mut host, 1));

    let mut fresh = SnapshotBaseline::default();
    assert!(!fresh.primed(), "a new baseline has nothing to decode against");

    let delta = send.reduce(&full(&mut host, 2), &SnapshotSendRates::default());
    assert!(!delta.keyframe);
    // The client plugin drops it on exactly this test; absorbing it anyway would produce a world
    // with no Pos at all, which is the failure being guarded.
    let components = fresh.absorb(&delta);
    assert!(
        components.values().all(|entities| entities.is_empty()),
        "a delta absorbed into an empty baseline reconstructs nothing, which is why it is dropped"
    );
}

/// A session reset must not leave the last session's state to decode the next one's deltas
/// against.
#[test]
fn resetting_a_baseline_forgets_the_previous_session() {
    let mut host = peer();
    host.world_mut().spawn((TickTrackedEntity(1), Pos(5)));

    let mut baseline = SnapshotBaseline::default();
    baseline.prime(&full(&mut host, 1));
    assert!(baseline.primed());

    baseline.reset();
    assert!(
        !baseline.primed(),
        "after a reset the next delta is refused until a keyframe arrives"
    );
}

fn entity_of(app: &mut App, id: u64) -> Entity {
    let mut q = app.world_mut().query::<(Entity, &TickTrackedEntity)>();
    q.iter(app.world())
        .find(|(_, tracked)| tracked.0 == id)
        .map(|(entity, _)| entity)
        .expect("no tracked entity with that id")
}

/// `input_margins` survives reduction — it is per-recipient state that has nothing to do with
/// what changed, and dropping it would stop every client's lead from adapting.
#[test]
fn a_delta_still_carries_the_input_margins() {
    let mut host = peer();
    host.world_mut().spawn((TickTrackedEntity(1), Pos(0)));

    let mut send = SnapshotBaseline::default();
    send.prime(&full(&mut host, 1));

    let mut second = full(&mut host, 2);
    second.input_margins = HashMap::from([(7u128, 2i64)]);
    let delta = send.reduce(&second, &SnapshotSendRates::default());

    assert_eq!(delta.input_margins.get(&7), Some(&2));
}
