//! What a snapshot says when a component, or an entity, is *not* there.
//!
//! `shooting_ropes/docs/upstream-needs.md` §3.5 claims a component the authority
//! removes is never replicated, and that the local rollback path and the snapshot
//! path disagree about it. Both were true, and both are fixed by the `stale` block
//! at the end of `deserialize_and_apply_component`.
//!
//! **To reproduce the bug**: delete that block. `a_removed_component_is_replicated`
//! and `the_snapshot_path_and_the_rollback_path_agree` then fail, and nothing else
//! does. The last three tests document limits the fix does *not* remove, so that
//! nobody reads "removal replicates now" as more than it is.

use bevy::prelude::*;
use bevy_ticked::prelude::TickedAppExt;
use bevy_ticked::registry::TickedComponentRegistry;
use bevy_ticked::tick::CurrentTick;
use bevy_ticked::tracked_entity::{TickTrackedEntity, TickTrackedEntityCounter};
use bevy_ticked_networking::prelude::*;
use bevy_ticked_networking::snapshot::{apply_snapshot, build_snapshot};
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
        .init_resource::<TickTrackedEntityCounter>()
        .register_networked_ticked_component::<Pos>()
        .register_networked_ticked_component::<Ride>()
        .register_ticked_component::<LocalOnly>();
    app
}

fn capture(app: &mut App, tick: u64) {
    let registry = app.world().resource::<TickedComponentRegistry>().clone();
    registry.capture_all(app.world_mut(), tick);
}

fn sync(host: &mut App, client: &mut App, tick: u64) {
    capture(host, tick);
    let snapshot = build_snapshot(host.world_mut(), tick);
    apply_snapshot(client.world_mut(), &snapshot);
}

fn only<C: Component + Copy>(app: &mut App) -> Option<C> {
    let mut q = app.world_mut().query::<&C>();
    q.iter(app.world()).next().copied()
}

fn entity_with<C: Component>(app: &mut App) -> Entity {
    let mut q = app.world_mut().query_filtered::<Entity, With<C>>();
    q.iter(app.world()).next().unwrap()
}

fn entity_by_id(app: &mut App, id: u64) -> Entity {
    let mut q = app.world_mut().query::<(Entity, &TickTrackedEntity)>();
    q.iter(app.world())
        .find(|(_, tracked)| tracked.0 == id)
        .map(|(entity, _)| entity)
        .expect("no tracked entity with that id")
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
        .index_of::<Ride>()
        .unwrap();
    let snapshot = build_snapshot(host.world_mut(), 1);

    assert_eq!(
        snapshot.components.get(&index).map(|m| m.len()),
        Some(0),
        "capture_component always calls set_tick, so serialize_all emits the type \
         with an empty map rather than omitting it"
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

/// A type registered *without* serialisation never appears in a snapshot, so
/// `deserialize_and_apply_component` is never called for it and nothing strips it.
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
/// just given: `apply_snapshot` inserts its components before `TickTrackedEntity`,
/// so it is not in the tracked set yet when the removal pass runs.
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

// ── limits the fix used to leave behind ──────────────────────────────────────

/// **Closed.** An entity whose networked components are all gone used to disappear entirely,
/// because existence was inferred from the union of the component maps — so "removal replicates"
/// stopped one tick short of stripping an entity bare, and a consumer that did it watched the
/// entity vanish from every peer while still holding it on the host.
///
/// `WorldSnapshot::entities` is the authority on existence now, so the two questions are asked
/// separately and answered separately.
#[test]
fn an_entity_stripped_of_every_networked_component_survives() {
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
        "the entity still exists on the client, because the host still has it"
    );
    assert!(host.world().get_entity(entity).is_ok(), "...as it does");

    // Stripped, not despawned: it is there and it carries nothing.
    let stripped = entity_by_id(&mut client, 2);
    let stripped = client.world().entity(stripped);
    assert!(stripped.get::<Pos>().is_none(), "Pos was removed, not merely stale");
    assert!(stripped.get::<Ride>().is_none(), "and so was Ride");
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
