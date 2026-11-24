//! A rewind across a contact id that changed hands replays bit-identically.
//!
//! avian reuses a freed `ContactId` for the next pair that starts touching. Here box A rests on the
//! ground when the window opens; A goes inside it, freeing its ground contact's id, and box B,
//! spawned afterwards, lands and takes that id. A rewind to the window's start tombstones B, and
//! avian's observer for a disabled body reads the contact graph *as it is then* — the window's
//! end, where the id is B's — and queues "stopped generating constraints" for it in
//! `ContactStatusChangeQueue`. The rewind puts the graphs back, where that id is A's ground
//! contact again; the queue is not a graph, and the next step took A's contact out of the solve.
//! A sank for a tick, and the replay diverged on its first tick (seen in run-2d: a ragdoll lying on
//! the floor, a death and a pickup in the same window).

use avian3d::prelude::*;
use bevy::prelude::*;
use bevy_ticked::TickedSimulation;
use bevy_ticked::lifetimes::TickedEntityCommandsExt;
use bevy_ticked::prelude::*;
use bevy_ticked::tracked_entity::{SpawnerSlot, TrackedSpawner, TrackedWorldExt};
use bevy_ticked_testing::fixtures::avian::{self, AvianHash, TickedSimulationSet};
use bevy_ticked_testing::prelude::*;

/// Everything has come to rest on the ground by then.
const FROM: Tick = Tick(90);
const DESPAWN_A_AT: Tick = Tick(95);
const SPAWN_B_AT: Tick = Tick(100);

#[derive(Component)]
struct BoxA;

fn despawn_a(tick: Res<CurrentTick>, a: Query<Entity, With<BoxA>>, mut commands: Commands) {
    if tick.0 == DESPAWN_A_AT
        && let Ok(entity) = a.single()
    {
        commands.entity(entity).despawn_ticked();
    }
}

fn spawn_b(tick: Res<CurrentTick>, mut spawner: TrackedSpawner) {
    if tick.0 == SPAWN_B_AT {
        spawner.spawn_by(
            SpawnerSlot::AUTHORITY,
            (
                RigidBody::Dynamic,
                Collider::cuboid(1.0, 1.0, 1.0),
                // In the tick, so `Position`: the simulation does not write transforms.
                Position::from_xyz(0.0, 0.6, 0.0),
            ),
        );
    }
}

fn app() -> App {
    let mut app = peer_app(HOST_UUID, |app| {
        avian::install(app);
        app.add_systems(
            TickedSimulation,
            (despawn_a, spawn_b).in_set(TickedSimulationSet::AfterPhysics),
        );
    });
    let world = app.world_mut();
    world.spawn((
        RigidBody::Static,
        Collider::cuboid(40.0, 1.0, 40.0),
        Transform::from_xyz(0.0, -0.5, 0.0),
    ));
    // A, and a box well away from everything so the ground has more than one contact.
    let a = world.spawn_tracked_by(
        SpawnerSlot::AUTHORITY,
        (
            RigidBody::Dynamic,
            Collider::cuboid(1.0, 1.0, 1.0),
            Transform::from_xyz(-4.0, 0.5, 0.0),
        ),
    );
    world.entity_mut(a).insert(BoxA);
    world.spawn_tracked_by(
        SpawnerSlot::AUTHORITY,
        (
            RigidBody::Dynamic,
            Collider::cuboid(1.0, 1.0, 1.0),
            Transform::from_xyz(4.0, 0.5, 0.0),
        ),
    );
    app
}

#[test]
fn a_rewind_across_a_contact_id_that_changed_hands_replays_identically() {
    let mut app = app();
    assert_replays_identically::<AvianHash>(&mut app, FROM, Ticks(40));
    let a_is_a_tombstone = app
        .world()
        .iter_entities()
        .any(|entity| entity.contains::<BoxA>() && entity.contains::<Tombstone>());
    assert!(a_is_a_tombstone, "the window did not despawn A");
}
