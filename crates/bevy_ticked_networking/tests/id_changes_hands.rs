//! A tracked id handed to a different thing, over the wire: the three ways it happens.
//!
//! An id outlives whatever first held it. The allocator is rolled back with everything else, so a
//! replay hands a dead pellet's id to whatever the corrected timeline spawns instead, and the
//! entity is reused on purpose — that is what keeps a game's `Entity` handles good across a
//! rewind. `lifetimes::reset` empties that entity and `redress` re-fires the game's
//! `Add<TickTrackedEntity>` observer. Three paths reach it, and these tests hold them apart:
//!
//! 1. a peer's **own** spawn mints the id again, which `SpawnedAs` tells apart from a replay of
//!    the same spawn — `bevy_ticked`'s `rollback_lifecycle.rs` covers that one;
//! 2. a record **revives a tombstone** the peer had left;
//! 3. a record renames an id the peer still holds **alive**.
//!
//! The third is the one worth writing down, because there is no local sign of it at all. The
//! record is only components; the marker never left, so `Add` does not fire; and comparing the
//! record's type mask against the entity's shape is what `SpawnedAs` documents as unreliable,
//! since a predicted spawn the authority has not seen loses its networked types to the absence
//! rule and so "changes shape" on every rollback. So the authority names those ids outright, in
//! `FullBody::reborn`.
//!
//! It is not an exotic corner either. Every snapshot arrives through the branch it lives in —
//! `apply_delta` rebuilds a `FullBody` and `apply_snapshot` hands it to `apply_full_body`, so "the
//! entity already exists" is the ordinary case. In a shooter it is reached by a client predicting
//! a shot on the tick the authority instead resolved that player's death: a player's own
//! projectiles and their own ragdoll pieces mint under the same slot, and because both happen
//! inside a single tick, no snapshot ever omits the id first for the peer to tombstone it.

use bevy::prelude::*;
use bevy_ticked::lifetimes::{TrackedEntityLifetimes, reset, tombstone};
use bevy_ticked::registry::TickedComponentRegistry;
use bevy_ticked::tick::CurrentTick;
use bevy_ticked::tracked_entity::{TickTrackedEntity, TrackedIdAllocator};
use bevy_ticked::tracked_index::TrackedEntityIndex;
use bevy_ticked_networking::prelude::*;
use bevy_ticked_networking::snapshot::{apply_full_body, build_full_body};
use serde::{Deserialize, Serialize};

const ID: u64 = 1;

/// What makes a thing a pellet. Nothing ever carries this *and* [`Piece`].
#[derive(Component, Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
struct Pellet(u64);

/// What makes a thing a piece of a body.
#[derive(Component, Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
struct Piece(u64);

#[derive(Component, Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
struct Pos(i32);

/// The game's presentation: local-only, hung on by the spawn observer and invisible to the wire.
/// A sprite, a mesh, a collider, a nameplate.
#[derive(Component, Debug, Default)]
struct Dressed;

/// Local-only, and *not* something the observer puts back. It is how these tests tell "never
/// reset" from "reset and then dressed again", which `Dressed` alone cannot.
#[derive(Component, Debug)]
struct Kept;

fn dress(add: On<Add, TickTrackedEntity>, mut commands: Commands) {
    commands.entity(add.entity).insert(Dressed);
}

fn peer() -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .init_resource::<TickedComponentRegistry>()
        .init_resource::<CurrentTick>()
        .init_resource::<TrackedIdAllocator>()
        .init_resource::<TrackedEntityIndex>()
        .init_resource::<TrackedEntityLifetimes>()
        .register_networked_ticked_component::<Pos>("Pos")
        .register_networked_ticked_component::<Pellet>("Pellet")
        .register_networked_ticked_component::<Piece>("Piece")
        .add_observer(dress);
    app
}

fn capture(app: &mut App, tick: u64) {
    let registry = app.world().resource::<TickedComponentRegistry>().clone();
    registry.capture_all(app.world_mut(), tick);
}

/// One snapshot, host to client, as `apply_snapshot` does it. `reborn` is what the authority says
/// has been handed to a different thing since the world this client acknowledged.
fn sync_reborn(host: &mut App, client: &mut App, tick: u64, reborn: &[u64]) {
    capture(host, tick);
    let mut body = build_full_body(host.world_mut(), tick);
    body.reborn = reborn.to_vec();
    apply_full_body(client.world_mut(), tick, &body);
}

/// A snapshot in which nothing changed hands, which is almost all of them.
fn sync(host: &mut App, client: &mut App, tick: u64) {
    sync_reborn(host, client, tick, &[]);
}

fn entity_of(app: &mut App, id: u64) -> Entity {
    let mut q = app.world_mut().query::<(Entity, &TickTrackedEntity)>();
    q.iter(app.world())
        .find(|(_, tracked)| tracked.0 == id)
        .map(|(entity, _)| entity)
        .expect("the peer holds that id")
}

/// Hand the id over on the authority: the pellet is gone and the id names a piece of a body.
fn hand_the_id_over(host: &mut App) {
    let on_host = entity_of(host, ID);
    host.world_mut().entity_mut(on_host).remove::<Pellet>();
    host.world_mut().entity_mut(on_host).insert(Piece(9));
}

/// The authority's side of the third path: `reset` is what records a change of hands, and the
/// query the broadcast builds `reborn` from offers only the ids a recipient has not heard about.
///
/// Recorded inside `reset` rather than at its call sites so that no path can empty an entity
/// without the wire learning of it — including the revive inside `apply_full_body`.
#[test]
fn reset_records_the_rebirth_and_the_query_is_baseline_relative() {
    let mut app = peer();
    app.world_mut()
        .spawn((TickTrackedEntity(ID), Pos(0), Pellet(5)));
    app.world_mut().resource_mut::<CurrentTick>().0 = 7;

    let entity = entity_of(&mut app, ID);
    let world = app.world_mut();
    reset(world, entity);

    let lifetimes = app.world().resource::<TrackedEntityLifetimes>();
    assert_eq!(
        lifetimes.reborn_since(6).collect::<Vec<_>>(),
        vec![ID],
        "a recipient whose world predates the change of hands has to be told about it"
    );
    assert!(
        lifetimes.reborn_since(7).next().is_none(),
        "and one whose baseline is the tick it happened on already has it. A needless reset is \
         not free: it would throw away local-only state and re-dress an entity already right"
    );
}

/// Path 2: the peer had already lost the id, so the record revives a tombstone. It needs no help
/// from the authority — a revive is a local sign in itself.
#[test]
fn an_id_revived_from_a_tombstone_as_something_else_is_reset_and_redressed() {
    let mut host = peer();
    let mut client = peer();
    host.world_mut()
        .spawn((TickTrackedEntity(ID), Pos(0), Pellet(5)));
    sync(&mut host, &mut client, 1);

    // What an earlier snapshot would have done to a prediction the authority never had: the id is
    // not in that body, so the client tombstones it.
    let before = entity_of(&mut client, ID);
    client.world_mut().entity_mut(before).insert(Kept);
    let world = client.world_mut();
    tombstone(world, before, ID, 1);

    hand_the_id_over(&mut host);
    sync(&mut host, &mut client, 2);

    let after = entity_of(&mut client, ID);
    assert_eq!(
        after, before,
        "the tombstone is reused, which is the premise"
    );
    assert_eq!(
        client.world().get::<Piece>(after),
        Some(&Piece(9)),
        "the record makes it a piece"
    );
    assert!(
        client.world().get::<Pellet>(after).is_none(),
        "and `reset` took the pellet's state off it"
    );
    assert!(
        client.world().get::<Kept>(after).is_none(),
        "including the local-only half, which is the whole point of emptying it"
    );
    assert!(
        client.world().get::<Dressed>(after).is_some(),
        "and the game was told to dress it again"
    );
}

/// Path 3: the peer still holds the id **alive**, and only the authority can say it has changed
/// hands. Before `reborn` this was a known limit: the replicated half was put right and the
/// entity kept the previous occupant's sprite, and in a physics game its collider.
#[test]
fn a_live_id_the_authority_names_as_reborn_is_reset_and_redressed() {
    let mut host = peer();
    let mut client = peer();
    host.world_mut()
        .spawn((TickTrackedEntity(ID), Pos(0), Pellet(5)));
    sync(&mut host, &mut client, 1);

    let entity = entity_of(&mut client, ID);
    assert!(
        client.world().get::<Dressed>(entity).is_some(),
        "the client dressed it as a pellet when the id first arrived"
    );
    // `Dressed` off, so that a second run of the observer is the only thing that could put it
    // back; `Kept` on, so that a reset is the only thing that could take it away.
    client.world_mut().entity_mut(entity).remove::<Dressed>();
    client.world_mut().entity_mut(entity).insert(Kept);

    hand_the_id_over(&mut host);
    sync_reborn(&mut host, &mut client, 2, &[ID]);

    let after = entity_of(&mut client, ID);
    assert_eq!(after, entity, "the same entity: the id is what is reused");
    assert_eq!(
        client.world().get::<Piece>(after),
        Some(&Piece(9)),
        "the record's own types land"
    );
    assert!(
        client.world().get::<Pellet>(after).is_none(),
        "the pellet's state is gone"
    );
    assert!(
        client.world().get::<Kept>(after).is_none(),
        "the entity was emptied, so the previous occupant's local-only state went with it"
    );
    assert!(
        client.world().get::<Dressed>(after).is_some(),
        "and the game was told to dress what the id has become. Without this a peer that only \
         ever heard about the id draws the dead thing for the rest of the session"
    );
}

/// The other side of it, and the reason `reborn` is per recipient rather than "every id that has
/// ever changed hands": an ordinary update must not empty anything. A reset throws away local-only
/// state the game is entitled to keep, and re-dressing an entity that was already right is work
/// that shows up as a nameplate handed out twice.
#[test]
fn a_live_id_nobody_named_is_left_alone() {
    let mut host = peer();
    let mut client = peer();
    host.world_mut()
        .spawn((TickTrackedEntity(ID), Pos(0), Pellet(5)));
    sync(&mut host, &mut client, 1);

    let entity = entity_of(&mut client, ID);
    client.world_mut().entity_mut(entity).insert(Kept);

    // The same thing, moved: what almost every snapshot is.
    let on_host = entity_of(&mut host, ID);
    host.world_mut().entity_mut(on_host).insert(Pos(1));
    sync(&mut host, &mut client, 2);

    let after = entity_of(&mut client, ID);
    assert_eq!(
        client.world().get::<Pos>(after),
        Some(&Pos(1)),
        "the update landed"
    );
    assert_eq!(
        client.world().get::<Pellet>(after),
        Some(&Pellet(5)),
        "and it is still a pellet"
    );
    assert!(
        client.world().get::<Kept>(after).is_some(),
        "nothing named this id, so nothing may empty it"
    );
}
