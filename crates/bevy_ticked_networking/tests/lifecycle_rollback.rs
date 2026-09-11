//! Spawns and despawns under correction: what a client keeps, what it drops, and what comes
//! back.

use std::time::Duration;

use bevy::ecs::entity_disabling::Disabled;
use bevy::prelude::*;
use bevy::time::TimeUpdateStrategy;
use bevy_ticked::prelude::*;
use bevy_ticked::registry::TickedComponentRegistry;
use bevy_ticked::tracked_entity::TickTrackedEntity;
use bevy_ticked_networking::client::{ClientTickBuffer, LocalClientPlayer};
use bevy_ticked_networking::diagnostics::HealthWarnings;
use bevy_ticked_networking::messages::ReceivedNetworkSnapshot;
use bevy_ticked_networking::prelude::*;
use bevy_ticked_networking::replication::ReplicationMode;
use bevy_ticked_networking::snapshot::{EntityRecord, FullBody, SnapshotPacket, build_full_body};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
struct Input {
    fire: bool,
}

#[derive(Component, Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
struct Pos(i32);

#[derive(Component, Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
struct Pellet;

const LOCAL: u128 = 7;
const TICK: Duration = Duration::from_micros(15_625);

/// Fire spawns a pellet under the local slot; the simulation of a predicted spawn.
fn fire(tick: Res<CurrentTick>, queue: Res<InputQueue<Input>>, mut spawner: TrackedSpawner) {
    if queue.get(tick.0, LOCAL).is_some_and(|input| input.fire) {
        spawner.spawn((Pos(0), Pellet, Owner(LOCAL), ReplicationMode::Predicted));
    }
}

fn client() -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .add_plugins(TickedPlugin {
            source: TickSource::Hz(64.0),
            ..default()
        })
        .add_plugins(TickedClientPlugin::<Input>::new())
        .insert_resource(TimeUpdateStrategy::ManualDuration(TICK))
        .register_networked_ticked_component::<Pos>("Pos")
        .register_networked_ticked_component::<Pellet>("Pellet")
        .add_systems(TickedSimulation, fire);
    app.insert_resource(LocalClientPlayer(LOCAL));
    app.insert_resource(LocalSpawnerSlot(SpawnerSlot(3)));
    app.update();
    app
}

fn wire<T: bevy_ticked::registry::TickedComponent>(app: &App) -> u16 {
    app.world()
        .resource::<TickedComponentRegistry>()
        .wire_index_of::<T>()
        .unwrap()
}

/// A body with the local player's body only.
fn body_with_player(app: &mut App, tick: u64) -> SnapshotPacket {
    let mut body = build_full_body(app.world_mut(), tick);
    let (pos, owner) = (wire::<Pos>(app), wire::<Owner>(app));
    body.entities.clear();
    body.put(
        EntityRecord::new(1 << 8)
            .with(pos, &Pos(0))
            .with(owner, &Owner(LOCAL)),
    );
    let mut packet = SnapshotPacket::full(tick, body);
    packet.your_margin = 2;
    packet
}

fn deliver(app: &mut App, packet: SnapshotPacket) {
    app.world_mut().trigger(ReceivedNetworkSnapshot(packet));
}

fn sync(app: &mut App) {
    let packet = body_with_player(app, 0);
    deliver(app, packet);
    app.update();
    for _ in 0..12 {
        app.update();
    }
}

fn pellets(app: &mut App) -> Vec<(Entity, u64)> {
    let mut q = app
        .world_mut()
        .query_filtered::<(Entity, &TickTrackedEntity), With<Pellet>>();
    q.iter(app.world()).map(|(e, t)| (e, t.0)).collect()
}

fn press_fire(app: &mut App) {
    let tick = app.world().resource::<CurrentTick>().0;
    app.world_mut().resource_mut::<InputQueue<Input>>().insert(
        tick + 1,
        LOCAL,
        Input { fire: true },
    );
    app.update();
}

fn lead(app: &App) -> u64 {
    app.world()
        .resource::<ClientTickBuffer>()
        .target_replay_distance
}

#[test]
fn a_predicted_spawn_is_minted_under_the_clients_slot() {
    let mut app = client();
    sync(&mut app);
    press_fire(&mut app);
    let minted = pellets(&mut app);
    assert_eq!(minted.len(), 1);
    let id = TickTrackedEntity(minted[0].1);
    assert_eq!(id.slot(), SpawnerSlot(3));
    assert_eq!(id.sequence(), 1);
}

#[test]
fn absence_is_authoritative_only_for_ids_spawned_at_or_before_the_snapshot_tick() {
    let mut app = client();
    sync(&mut app);
    press_fire(&mut app);
    let fired_at = app.world().resource::<CurrentTick>().0;
    let (entity, _) = pellets(&mut app)[0];

    // The authority's word for a tick before the shot: says nothing about the pellet.
    let behind = fired_at - lead(&app);
    let packet = body_with_player(&mut app, behind);
    deliver(&mut app, packet);
    app.update();
    assert_eq!(
        pellets(&mut app).len(),
        1,
        "a snapshot from before the shot leaves it"
    );
    assert!(app.world().get::<Disabled>(entity).is_none());

    // For a tick after the shot: the authority would have it, and does not.
    for _ in 0..8 {
        app.update();
    }
    let packet = body_with_player(&mut app, fired_at + 2);
    deliver(&mut app, packet);
    app.update();
    assert!(
        pellets(&mut app).is_empty(),
        "the host never confirmed the pellet, so it is gone from every query"
    );
    assert!(
        app.world().get::<Disabled>(entity).is_some(),
        "tombstoned, not destroyed, in case a later word brings it back"
    );
}

#[test]
fn a_predicted_despawn_the_host_contradicts_is_undone() {
    let mut app = client();
    sync(&mut app);
    let player = {
        let mut q = app.world_mut().query_filtered::<Entity, With<Owner>>();
        q.single(app.world()).unwrap()
    };
    app.world_mut().entity_mut(player).despawn_ticked();
    app.update();
    assert!(app.world().get::<Disabled>(player).is_some());

    let current = app.world().resource::<CurrentTick>().0;
    let behind = current - lead(&app);
    let packet = body_with_player(&mut app, behind);
    deliver(&mut app, packet);
    app.update();

    let mut q = app.world_mut().query_filtered::<Entity, With<Owner>>();
    let revived = q.single(app.world()).unwrap();
    assert_eq!(
        revived, player,
        "the authority still has it: the same entity is back"
    );
    assert!(app.world().get::<Disabled>(player).is_none());
}

#[test]
fn a_correction_from_before_the_shot_replays_the_shot_onto_the_same_entity() {
    let mut app = client();
    sync(&mut app);
    press_fire(&mut app);
    let (entity, id) = pellets(&mut app)[0];
    for _ in 0..3 {
        app.update();
    }
    // A correction from before the shot that moves the player: full rollback and replay. The
    // replay runs the fire input again, mints the same id, and revives the tombstone.
    let behind = app.world().resource::<CurrentTick>().0 - lead(&app);
    let mut packet = body_with_player(&mut app, behind);
    if let bevy_ticked_networking::snapshot::SnapshotBody::Full(body) = &mut packet.body {
        let (pos, owner) = (wire::<Pos>(&app), wire::<Owner>(&app));
        body.put(
            EntityRecord::new(1 << 8)
                .with(pos, &Pos(5))
                .with(owner, &Owner(LOCAL)),
        );
    }
    deliver(&mut app, packet);
    app.update();

    let after = pellets(&mut app);
    assert_eq!(after.len(), 1, "one pellet, not two: {after:?}");
    assert_eq!(
        after[0],
        (entity, id),
        "the entity the game already held, under the same id"
    );
}

#[test]
fn a_client_minting_under_the_authority_slot_is_caught() {
    let mut app = client();
    sync(&mut app);
    app.world_mut()
        .spawn_tracked_by(SpawnerSlot::AUTHORITY, (Pos(0), Pellet));
    app.update();
    assert_eq!(
        app.world()
            .resource::<HealthWarnings>()
            .client_minted_tracked_id,
        1
    );
    // Under its own slot is fine.
    app.world_mut().spawn_tracked((Pos(0), Pellet));
    app.update();
    assert_eq!(
        app.world()
            .resource::<HealthWarnings>()
            .client_minted_tracked_id,
        1
    );
}

#[test]
fn the_allocator_travels_and_raises_the_clients_authority_sequence() {
    let mut app = client();
    sync(&mut app);
    // The host has minted forty entities under slot 0 by now.
    let mut host_allocator = TrackedIdAllocator::default();
    for _ in 0..40 {
        host_allocator.next_authority();
    }
    let index = app
        .world()
        .resource::<bevy_ticked::resource_registry::TickedResourceRegistry>()
        .wire_index_of::<TrackedIdAllocator>()
        .expect("the allocator is on the wire");
    let current = app.world().resource::<CurrentTick>().0;
    let behind = current - lead(&app);
    let mut packet = body_with_player(&mut app, behind);
    if let bevy_ticked_networking::snapshot::SnapshotBody::Full(body) = &mut packet.body {
        body.resources = vec![(index, postcard::to_allocvec(&host_allocator).unwrap())];
    }
    deliver(&mut app, packet);
    app.update();
    assert_eq!(
        app.world()
            .resource::<TrackedIdAllocator>()
            .peek(SpawnerSlot::AUTHORITY),
        41,
        "the client's idea of the authority's next id follows the snapshot"
    );
}

#[test]
fn an_entity_carried_by_the_snapshot_is_born_at_or_before_its_tick() {
    let mut app = client();
    sync(&mut app);
    let lifetimes = app.world().resource::<TrackedEntityLifetimes>();
    let born = lifetimes
        .born_at(1 << 8)
        .expect("the player is in the lifetimes");
    assert_eq!(born, 0, "the initial sync named it at tick 0");
    let _ = FullBody::default();
}
