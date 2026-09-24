//! Two peers that disagree about what was spawned must not end up disagreeing about what an id
//! *is*.
//!
//! A client predicts. Whenever its prediction spawns something the authority did not — a guessed
//! remote input, a pad the authority alone refills — the two have minted different things, and
//! before ids were keyed by shape they minted them from the same counter. Every id after the
//! disagreement named a different thing on each side, and the client still held its own occupant
//! *alive* when the snapshot named the id: nothing on either side had a reason to say it changed
//! hands, so the record was decoded onto the old occupant and the game's presentation never ran
//! again. A gun drawn as a grenade's blast, and the blast never going away.
//!
//! These tests diverge the two peers for real — no `reborn` filled in by hand — and check that
//! each thing lands on an entity dressed for it.

use bevy::prelude::*;
use bevy_ticked::lifetimes::TrackedEntityLifetimes;
use bevy_ticked::registry::TickedComponentRegistry;
use bevy_ticked::tick::CurrentTick;
use bevy_ticked::tracked_entity::{
    SpawnerSlot, TickTrackedEntity, TrackedIdAllocator, TrackedWorldExt,
};
use bevy_ticked::tracked_index::TrackedEntityIndex;
use bevy_ticked_networking::prelude::*;
use bevy_ticked_networking::snapshot::{apply_full_body, build_full_body};
use serde::{Deserialize, Serialize};

#[derive(Component, Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
struct Pos(i32);

/// A weapon lying on a pad. Only the authority refills pads.
#[derive(Component, Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
struct Gun(u8);

/// A grenade's blast. Every peer predicts it.
#[derive(Component, Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
struct Blast(u8);

#[derive(Component, Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
struct Pellet(u8);

#[derive(Component, Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
struct Piece(u8);

/// What the game's spawn observer hangs on: the sprite. Local-only.
#[derive(Component, Debug)]
struct Dressed;

/// Local-only and never put back by the observer: present afterwards only on an entity nobody
/// emptied.
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
        .register_networked_ticked_component::<Gun>("Gun")
        .register_networked_ticked_component::<Blast>("Blast")
        .register_networked_ticked_component::<Pellet>("Pellet")
        .register_networked_ticked_component::<Piece>("Piece")
        .add_observer(dress);
    app
}

fn sync(host: &mut App, client: &mut App, tick: u64) {
    let registry = host.world().resource::<TickedComponentRegistry>().clone();
    registry.capture_all(host.world_mut(), tick);
    let body = build_full_body(host.world_mut(), tick);
    apply_full_body(client.world_mut(), tick, &body);
}

fn id_of(app: &mut App, entity: Entity) -> u64 {
    app.world().get::<TickTrackedEntity>(entity).unwrap().0
}

fn holding<T: Component>(app: &mut App) -> Vec<Entity> {
    let mut query = app.world_mut().query_filtered::<Entity, With<T>>();
    query.iter(app.world()).collect()
}

/// The report: a host refills a pad the client does not predict, then a grenade the client does
/// predict goes off. Both are minted under the same slot.
#[test]
fn a_spawn_only_the_authority_makes_does_not_shift_the_ids_the_client_predicts() {
    let mut host = peer();
    let mut client = peer();

    host.world_mut()
        .spawn_tracked_by(SpawnerSlot::AUTHORITY, (Pos(0), Gun(1)));
    let host_blast = host
        .world_mut()
        .spawn_tracked_by(SpawnerSlot::AUTHORITY, (Pos(5), Blast(0)));
    let predicted = client
        .world_mut()
        .spawn_tracked_by(SpawnerSlot::AUTHORITY, (Pos(5), Blast(0)));
    client.world_mut().flush();
    client.world_mut().entity_mut(predicted).insert(Kept);

    assert_eq!(
        id_of(&mut client, predicted),
        id_of(&mut host, host_blast),
        "the blast is minted under the same id on both peers, the pad in between notwithstanding"
    );

    sync(&mut host, &mut client, 1);
    client.world_mut().flush();

    assert_eq!(
        holding::<Blast>(&mut client),
        vec![predicted],
        "the authority's blast is confirmed onto the entity the client already drew"
    );
    assert!(
        client.world().get::<Kept>(predicted).is_some(),
        "and it was not emptied: it is the same thing"
    );
    let guns = holding::<Gun>(&mut client);
    assert_eq!(guns.len(), 1, "the gun arrives");
    let gun = guns[0];
    assert_ne!(gun, predicted, "on an entity of its own, not on the blast");
    assert!(
        client.world().get::<Dressed>(gun).is_some(),
        "dressed as whatever it is"
    );
    assert!(client.world().get::<Kept>(gun).is_none());
}

/// A client guesses a remote player kept shooting; the authority, which had the real input,
/// resolved their death instead. Same slot, different things.
#[test]
fn a_mispredicted_spawn_is_not_decoded_onto_by_what_the_authority_spawned() {
    let mut host = peer();
    let mut client = peer();
    let slot = SpawnerSlot(3);

    host.world_mut().spawn_tracked_by(slot, (Pos(0), Piece(1)));
    let guessed = client
        .world_mut()
        .spawn_tracked_by(slot, (Pos(9), Pellet(2)));
    client.world_mut().flush();
    client.world_mut().entity_mut(guessed).insert(Kept);

    sync(&mut host, &mut client, 1);
    client.world_mut().flush();

    assert!(
        holding::<Pellet>(&mut client).is_empty(),
        "the authority never had the pellet, so it is gone"
    );
    let pieces = holding::<Piece>(&mut client);
    assert_eq!(pieces.len(), 1);
    assert!(
        client.world().get::<Kept>(pieces[0]).is_none(),
        "and the piece is not the pellet's entity wearing new state: nothing of the pellet is on it"
    );
    assert!(client.world().get::<Dressed>(pieces[0]).is_some());
}

/// The stream is named by the wire, not by the build: two peers that registered the same types in
/// a different order still mint the same id for the same thing.
#[test]
fn the_same_bundle_mints_the_same_id_whatever_the_registration_order() {
    let mut first = peer();
    let mut second = App::new();
    second
        .add_plugins(MinimalPlugins)
        .init_resource::<TickedComponentRegistry>()
        .init_resource::<CurrentTick>()
        .init_resource::<TrackedIdAllocator>()
        .init_resource::<TrackedEntityIndex>()
        .init_resource::<TrackedEntityLifetimes>()
        .register_networked_ticked_component::<Piece>("Piece")
        .register_networked_ticked_component::<Pellet>("Pellet")
        .register_networked_ticked_component::<Blast>("Blast")
        .register_networked_ticked_component::<Gun>("Gun")
        .register_networked_ticked_component::<Pos>("Pos");

    let a = first
        .world_mut()
        .spawn_tracked_by(SpawnerSlot(2), (Blast(0), Pos(1)));
    let b = second
        .world_mut()
        .spawn_tracked_by(SpawnerSlot(2), (Pos(1), Blast(0)));
    assert_eq!(id_of(&mut first, a), id_of(&mut second, b));
}

/// Different things minted under one slot get different streams; the same thing keeps counting.
#[test]
fn a_stream_counts_one_kind_of_thing() {
    let mut app = peer();
    let gun = app
        .world_mut()
        .spawn_tracked_by(SpawnerSlot::AUTHORITY, (Pos(0), Gun(1)));
    let blast = app
        .world_mut()
        .spawn_tracked_by(SpawnerSlot::AUTHORITY, (Pos(0), Blast(1)));
    let second_gun = app
        .world_mut()
        .spawn_tracked_by(SpawnerSlot::AUTHORITY, (Pos(0), Gun(2)));
    let [gun, blast, second_gun] =
        [gun, blast, second_gun].map(|e| TickTrackedEntity(id_of(&mut app, e)));

    assert_ne!(gun.stream(), blast.stream());
    assert_eq!(gun.stream(), second_gun.stream());
    assert_eq!(gun.sequence(), 1);
    assert_eq!(blast.sequence(), 1);
    assert_eq!(second_gun.sequence(), 2);
    assert!(
        [gun, blast, second_gun]
            .iter()
            .all(|id| id.slot() == SpawnerSlot::AUTHORITY)
    );
}
