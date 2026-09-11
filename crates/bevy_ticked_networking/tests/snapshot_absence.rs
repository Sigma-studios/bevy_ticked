//! What a snapshot says when a component, or an entity, is *not* there.
//!
//! `shooting_ropes/docs/upstream-needs.md` §3.5 claims a component the authority
//! removes is never replicated, and that the local rollback path and the snapshot
//! path disagree about it. Both were true, and both are fixed by `finish_tick` in
//! `networked_registry.rs`, which strips a type from every tracked entity the snapshot did not
//! give it to.
//!
//! **To reproduce the bug**: make `finish_tick` a no-op. `a_removed_component_is_replicated`
//! and `the_snapshot_path_and_the_rollback_path_agree` then fail, and nothing else
//! does. The last three tests document limits the fix does *not* remove, so that
//! nobody reads "removal replicates now" as more than it is.

use bevy::prelude::*;
use bevy_ticked::prelude::TickedAppExt;
use bevy_ticked::registry::TickedComponentRegistry;
use bevy_ticked::tick::CurrentTick;
use bevy_ticked::tracked_entity::{TickTrackedEntity, TrackedIdAllocator};
use bevy_ticked_networking::prelude::*;
use bevy_ticked_networking::snapshot::{apply_full_body, build_full_body};
use serde::{Deserialize, Serialize};

/// Stands in for `shooting_ropes`'s `Ride`: inserted while something is happening,
/// removed when it stops. It is the only networked component either consumer
/// removes rather than blanking with a sentinel.
#[derive(Component, Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
struct Ride(u64);

#[derive(Component, Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
struct Pos(i32);

/// Registered for rollback but never serialised -- the shape of
/// `CharacterControllerState`, `JumpLatch`, `DialRepeat` and `SwingCooldown`.
#[derive(Component, Clone, Copy, Debug, PartialEq)]
struct LocalOnly(i32);

fn peer() -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .init_resource::<TickedComponentRegistry>()
        .init_resource::<CurrentTick>()
        .init_resource::<TrackedIdAllocator>()
        .register_networked_ticked_component::<Pos>("Pos")
        .register_networked_ticked_component::<Ride>("Ride")
        .register_ticked_component::<LocalOnly>();
    app
}

fn capture(app: &mut App, tick: u64) {
    let registry = app.world().resource::<TickedComponentRegistry>().clone();
    registry.capture_all(app.world_mut(), tick);
}

fn sync(host: &mut App, client: &mut App, tick: u64) {
    capture(host, tick);
    let body = build_full_body(host.world_mut(), tick);
    apply_full_body(client.world_mut(), tick, &body);
}

fn only<C: Component + Copy>(app: &mut App) -> Option<C> {
    let mut q = app.world_mut().query::<&C>();
    q.iter(app.world()).next().copied()
}

fn entity_with<C: Component>(app: &mut App) -> Entity {
    let mut q = app.world_mut().query_filtered::<Entity, With<C>>();
    q.iter(app.world()).next().unwrap()
}

fn tracked_ids(app: &mut App) -> Vec<u64> {
    let mut q = app.world_mut().query::<&TickTrackedEntity>();
    let mut ids: Vec<u64> = q.iter(app.world()).map(|t| t.0).collect();
    ids.sort_unstable();
    ids
}

// ── §3.5 ─────────────────────────────────────────────────────────────────────

/// The information was never missing from the wire -- it was discarded on arrival.
#[test]
fn the_snapshot_already_says_that_nobody_is_riding() {
    let mut host = peer();
    host.world_mut().spawn((TickTrackedEntity(1), Pos(0)));
    capture(&mut host, 1);

    let index = host
        .world()
        .resource::<TickedComponentRegistry>()
        .wire_index_of::<Ride>()
        .unwrap();
    let body = build_full_body(host.world_mut(), 1);

    let record = body.record(1).expect("the body has the entity");
    assert!(
        !record.present.contains(index),
        "the record's mask says which types the entity carries, and Ride is not among them: \
         the absence is in the packet, not inferred from silence"
    );
}

#[test]
fn a_removed_component_is_replicated() {
    let mut host = peer();
    let mut client = peer();

    host.world_mut().spawn((TickTrackedEntity(1), Pos(0), Ride(7)));
    sync(&mut host, &mut client, 1);
    assert_eq!(only::<Ride>(&mut client), Some(Ride(7)), "the ride arrived");

    let entity = entity_with::<Ride>(&mut host);
    host.world_mut().entity_mut(entity).remove::<Ride>();
    host.world_mut().entity_mut(entity).insert(Pos(1));
    sync(&mut host, &mut client, 2);

    assert_eq!(
        only::<Ride>(&mut client),
        None,
        "the host ended the ride, so the client must too"
    );
    assert_eq!(
        only::<Pos>(&mut client),
        Some(Pos(1)),
        "and everything else in the same snapshot still lands"
    );
}

/// The sharper half: before the fix the client's *history* recorded the removal
/// while its *world* did not, so which answer a consumer got depended on how the
/// tick happened to be reached.
#[test]
fn the_snapshot_path_and_the_rollback_path_agree() {
    let mut host = peer();
    let mut client = peer();

    host.world_mut().spawn((TickTrackedEntity(1), Pos(0), Ride(7)));
    sync(&mut host, &mut client, 1);

    let entity = entity_with::<Ride>(&mut host);
    host.world_mut().entity_mut(entity).remove::<Ride>();
    sync(&mut host, &mut client, 2);
    let after_snapshot = only::<Ride>(&mut client);

    let registry = client.world().resource::<TickedComponentRegistry>().clone();
    registry.restore_all(client.world_mut(), 2);
    let after_rollback = only::<Ride>(&mut client);

    assert_eq!(after_snapshot, None);
    assert_eq!(after_rollback, after_snapshot, "the two paths must not disagree");
}

// ── what the fix must not break ──────────────────────────────────────────────

/// A type registered *without* serialisation is not on the wire, so no record names it and
/// nothing strips it.
/// "The authority said nothing" and "the authority has none" stay distinct.
#[test]
fn a_rollback_only_component_is_never_stripped_by_a_snapshot() {
    let mut host = peer();
    let mut client = peer();

    host.world_mut().spawn((TickTrackedEntity(1), Pos(0)));
    sync(&mut host, &mut client, 1);

    let entity = entity_with::<Pos>(&mut client);
    client.world_mut().entity_mut(entity).insert(LocalOnly(42));
    sync(&mut host, &mut client, 2);

    assert_eq!(
        client.world().entity(entity).get::<LocalOnly>().copied(),
        Some(LocalOnly(42)),
        "a snapshot must not remove what it never carried"
    );
}

/// A *new* entity arriving in the same snapshot as a removal keeps what it was
/// just given: its components are recorded at the tick like anybody else's, so the removal
/// pass has nothing to strip from it.
#[test]
fn an_entity_spawned_by_the_same_snapshot_keeps_its_components() {
    let mut host = peer();
    let mut client = peer();

    host.world_mut().spawn((TickTrackedEntity(1), Pos(0)));
    sync(&mut host, &mut client, 1);

    host.world_mut().spawn((TickTrackedEntity(2), Pos(9), Ride(3)));
    sync(&mut host, &mut client, 2);

    let mut q = client.world_mut().query::<(&TickTrackedEntity, Option<&Ride>)>();
    let mut seen: Vec<(u64, Option<u64>)> =
        q.iter(client.world()).map(|(t, r)| (t.0, r.map(|r| r.0))).collect();
    seen.sort();
    assert_eq!(
        seen,
        vec![(1, None), (2, Some(3))],
        "the newcomer keeps its Ride and the incumbent still has none"
    );
}

// ── limits the fix does not remove ───────────────────────────────────────────

/// An entity stripped of every networked component survives: the wire is entity-major, so a
/// record with an empty mask is still a record, and the entity still exists. The old
/// type-major shape derived existence from the union of the component maps, and a bare
/// tracked entity vanished from every client one tick after being stripped.
#[test]
fn an_entity_stripped_of_every_networked_component_survives_bare() {
    let mut host = peer();
    let mut client = peer();

    host.world_mut().spawn((TickTrackedEntity(1), Pos(0)));
    host.world_mut().spawn((TickTrackedEntity(2), Pos(0), Ride(7)));
    sync(&mut host, &mut client, 1);
    assert_eq!(tracked_ids(&mut client), vec![1, 2]);

    let entity = entity_with::<Ride>(&mut host);
    host.world_mut().entity_mut(entity).remove::<Ride>();
    host.world_mut().entity_mut(entity).remove::<Pos>();
    sync(&mut host, &mut client, 2);

    assert_eq!(
        tracked_ids(&mut client),
        vec![1, 2],
        "a bare tracked entity is still an entity on every peer"
    );
    let bare = client
        .world_mut()
        .query::<(&TickTrackedEntity, Option<&Pos>, Option<&Ride>)>()
        .iter(client.world())
        .find(|(t, _, _)| t.0 == 2)
        .map(|(_, pos, ride)| (pos.copied(), ride.copied()));
    assert_eq!(bare, Some((None, None)), "with nothing on it");
    assert!(host.world().get_entity(entity).is_ok(), "...and the host still has it");
}

/// §3.3's precondition, checked rather than assumed: a tracked entity whose whole
/// life falls between two snapshots is invisible to a peer. `bevy_kart` spawns
/// exactly this shape -- `EntityKind::Explosion`, a tracked entity that exists only
/// so every peer fires a sound -- so this is live across the consumer set, not
/// hypothetical.
#[test]
fn an_entity_that_lives_between_two_snapshots_is_never_seen() {
    let mut host = peer();
    let mut client = peer();

    host.world_mut().spawn((TickTrackedEntity(1), Pos(0)));
    sync(&mut host, &mut client, 1);

    let flash = host.world_mut().spawn((TickTrackedEntity(2), Pos(5))).id();
    capture(&mut host, 2);
    host.world_mut().despawn(flash);
    sync(&mut host, &mut client, 3);

    assert_eq!(
        tracked_ids(&mut client),
        vec![1],
        "the client never learns the explosion happened"
    );
}
