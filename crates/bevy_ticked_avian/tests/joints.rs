//! Joints under the bundle. `JointGraph` is rolled back with the bodies, and nothing else here
//! puts a joint in a replay: a jointed chain replays bit-identically, and a rewind across the
//! tick a jointed pair was spawned, or the tick it was tombstoned, neither panics nor diverges.
//!
//! Bodies are placed by `Position` and `Rotation` in the spawn bundle rather than by `Transform`.
//! That is the documented rule, and it matters more here than anywhere: a spawn replayed over a
//! tombstone revives the same entity, and re-inserting `RigidBody` on it is an insert, not an
//! add — a `Transform` would not be read again.

use avian3d::prelude::*;
use bevy::prelude::*;
use bevy_ticked::TickedSimulation;
use bevy_ticked::lifetimes::TickedEntityCommandsExt;
use bevy_ticked::prelude::*;
use bevy_ticked::tracked_entity::{SpawnerSlot, TrackedSpawner, TrackedWorldExt};
use bevy_ticked_testing::fixtures::avian::{self, AvianHash, TickedSimulationSet};
use bevy_ticked_testing::prelude::*;

const LINKS: usize = 4;
/// Half a link's length: each joint sits on the end of one link and the start of the next.
const HALF_LINK: f32 = 0.5;

/// When the pair in the lifecycle tests is minted, and when it is tombstoned.
const SPAWN_AT: u64 = 30;
const TOMBSTONE_AT: u64 = 80;

/// A revolute joint from the end of `parent` to the start of `child`, with limits the chain folds
/// against when it lands.
fn link(parent: Entity, child: Entity) -> RevoluteJoint {
    RevoluteJoint::new(parent, child)
        .with_local_anchor1(Vec3::X * HALF_LINK)
        .with_local_anchor2(Vec3::NEG_X * HALF_LINK)
        .with_angle_limits(-1.0, 1.0)
}

/// A dynamic link at `at`, placed the way a game under rollback places one.
fn body(at: Vec3) -> impl Bundle {
    (
        RigidBody::Dynamic,
        Collider::cuboid(HALF_LINK * 2.0, 0.25, 0.25),
        Position(at),
        Rotation::default(),
        LinearVelocity::ZERO,
        AngularVelocity::ZERO,
    )
}

fn spawn_ground(world: &mut World) {
    world.spawn((
        RigidBody::Static,
        Collider::cuboid(40.0, 1.0, 40.0),
        Position(Vec3::new(0.0, -0.5, 0.0)),
    ));
}

/// A horizontal chain of `links` links, jointed end to end and held up by nothing: it swings
/// down, the free end hits the ground and the rest folds onto it against the limits. Contacts,
/// joints and limits in one solve. Returns the bodies, top first.
fn spawn_chain(world: &mut World, links: usize) -> Vec<Entity> {
    spawn_ground(world);
    let mut bodies: Vec<Entity> = Vec::with_capacity(links);
    for i in 0..links {
        let at = Vec3::new(HALF_LINK * 2.0 * i as f32, 3.0 + 0.2 * i as f32, 0.0);
        let entity = world.spawn_tracked_by(SpawnerSlot::AUTHORITY, body(at));
        if let Some(&parent) = bodies.last() {
            world.spawn_tracked_by(SpawnerSlot::AUTHORITY, link(parent, entity));
        }
        bodies.push(entity);
    }
    bodies
}

/// How far apart the two halves of the joint between `parent` and `child` have come.
fn joint_gap(world: &World, parent: Entity, child: Entity) -> f32 {
    let anchor = |entity: Entity, local: Vec3| {
        let position = world
            .get::<Position>(entity)
            .expect("a link has a position");
        let rotation = world
            .get::<Rotation>(entity)
            .expect("a link has a rotation");
        position.0 + rotation.0 * local
    };
    anchor(parent, Vec3::X * HALF_LINK).distance(anchor(child, Vec3::NEG_X * HALF_LINK))
}

/// The chain swinging down and landing replays bit-identically, and the joints are real: after
/// it has landed, every pair of anchors is still together.
#[test]
fn a_jointed_chain_replays_bit_identically() {
    let mut app = peer_app(HOST_UUID, avian::install);
    let bodies = spawn_chain(app.world_mut(), LINKS);
    // Mid-swing to landed: the free end strikes the ground inside the window.
    assert_replays_identically::<AvianHash>(&mut app, 20, 96);

    let free_end = bodies[LINKS - 1];
    let started_at = Vec3::new(
        HALF_LINK * 2.0 * (LINKS - 1) as f32,
        3.0 + 0.2 * (LINKS - 1) as f32,
        0.0,
    );
    assert!(
        app.world()
            .get::<Position>(free_end)
            .unwrap()
            .0
            .distance(started_at)
            > 0.5,
        "the chain did not move; the replay compared a still world"
    );
    for pair in bodies.windows(2) {
        let gap = joint_gap(app.world(), pair[0], pair[1]);
        assert!(
            gap < 0.05,
            "the joint between {:?} and {:?} has come {gap} apart: it is not in the solve",
            pair[0],
            pair[1]
        );
    }
}

/// Marks the pair the lifecycle systems mint and tombstone.
#[derive(Component)]
struct Pair;

/// A jointed pair, minted on [`SPAWN_AT`] under the authority's slot from inside the tick — the
/// way a game spawns a ragdoll on a death — so a replay across that tick mints it again.
fn spawn_pair(tick: Res<CurrentTick>, mut spawner: TrackedSpawner) {
    if tick.0 != SPAWN_AT {
        return;
    }
    let a = spawner.spawn_by(
        SpawnerSlot::AUTHORITY,
        (Pair, body(Vec3::new(-4.0, 2.0, 0.0))),
    );
    let b = spawner.spawn_by(
        SpawnerSlot::AUTHORITY,
        (Pair, body(Vec3::new(-3.0, 2.5, 0.0))),
    );
    spawner.spawn_by(SpawnerSlot::AUTHORITY, (Pair, link(a, b)));
}

/// The pair and its joint, tombstoned together on [`TOMBSTONE_AT`].
fn tombstone_pair(
    tick: Res<CurrentTick>,
    pairs: Query<Entity, With<Pair>>,
    mut commands: Commands,
) {
    if tick.0 != TOMBSTONE_AT {
        return;
    }
    for entity in &pairs {
        commands.entity(entity).despawn_ticked();
    }
}

/// A peer with the chain, and the pair's lifecycle running inside the tick.
fn lifecycle_app() -> App {
    let mut app = peer_app(HOST_UUID, |app| {
        avian::install(app);
        app.add_systems(
            TickedSimulation,
            (spawn_pair, tombstone_pair)
                .chain()
                .in_set(TickedSimulationSet::BeforePhysics),
        );
    });
    spawn_chain(app.world_mut(), LINKS);
    app
}

/// The pair's two bodies and its joint all exist and are all tombstoned — so the window really
/// did contain the spawn and the tombstone, rather than agreeing about a pair that never was.
fn assert_the_pair_was_tombstoned(app: &App) {
    let (live, tombstoned) = app
        .world()
        .iter_entities()
        .filter(|entity| entity.contains::<Pair>())
        .fold((0, 0), |(live, dead), entity| {
            if entity.contains::<Tombstone>() {
                (live, dead + 1)
            } else {
                (live + 1, dead)
            }
        });
    assert_eq!(
        (live, tombstoned),
        (0, 3),
        "expected the pair's two bodies and joint, all tombstoned"
    );
}

/// A rewind to before the pair existed undoes its spawn and its tombstone; the replay mints it
/// again onto the same entities and tombstones it again, with the joint back in the graph in
/// between.
#[test]
fn a_rewind_across_a_jointed_spawn_and_its_tombstone_replays_identically() {
    let mut app = lifecycle_app();
    assert_replays_identically::<AvianHash>(&mut app, SPAWN_AT - 10, 96);
    assert_the_pair_was_tombstoned(&app);
}

/// A rewind to while the pair was alive revives the tombstoned bodies and their joint, and the
/// replay holds them together until they are tombstoned again.
#[test]
fn a_rewind_across_a_jointed_tombstone_replays_identically() {
    let mut app = lifecycle_app();
    assert_replays_identically::<AvianHash>(&mut app, TOMBSTONE_AT - 20, 64);
    assert_the_pair_was_tombstoned(&app);
}
