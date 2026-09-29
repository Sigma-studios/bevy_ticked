//! avian's result does not depend on the order bodies were spawned in.
//!
//! A client never builds its world in the host's order: `snapshot.rs` spawns what a snapshot
//! names in id order, a rollback revives tombstones in `HashMap` order (`lifetimes.rs`), and the
//! host's own tables are spawn order shuffled by every `swap_remove` a despawn did. Left to
//! itself avian orients each contact pair and colours the constraint graph in the order the
//! broad phase found the pairs, which is the order the colliders were spawned in: the same
//! bodies at the same places, spawned in reverse, part in the seventh tick (the first
//! body-on-body contact), in 2d and 3d. A client's prediction of a contact could then never be
//! bit-identical to the host's, and `prediction_matches` would fail on every snapshot covering
//! one.
//!
//! Two worlds, the same bodies at the same places under the same tracked ids -- as a snapshot
//! would name them -- spawned in different orders, stepped 256 ticks, compared by a label
//! (never by `Entity`) every tick. The plugin's canonical contact order makes them agree bit for
//! bit; the negative control turns it off and watches them part.
//!
//! The 2d half needs the crate's `2d` feature:
//! `cargo test -p bevy_ticked_avian --features 2d --test spawn_order_independence`.

use bevy::prelude::*;
use bevy_ticked::tracked_entity::{SpawnerSlot, TickTrackedEntity, TrackedIdAllocator};
use bevy_ticked_testing::prelude::*;

/// Which body this is, independent of spawn order and `Entity`.
#[derive(Component, Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct Label(u32);

const BOXES: u32 = 8;
const TICKS: usize = 256;

/// Spawn orders to compare against `0..n`.
#[derive(Clone, Copy, Debug)]
enum Order {
    Forward,
    Reverse,
    /// Forward, with untracked filler spawned and despawned in between, so every body's
    /// `Entity` index differs while the table order stays the same.
    ForwardShiftedEntities,
    /// Evens, then odds.
    Interleaved,
}

fn permutation(order: Order, n: u32) -> Vec<u32> {
    match order {
        Order::Forward | Order::ForwardShiftedEntities => (0..n).collect(),
        Order::Reverse => (0..n).rev().collect(),
        Order::Interleaved => (0..n).step_by(2).chain((1..n).step_by(2)).collect(),
    }
}

/// Spawn `bundle` under the tracked id `label` names, as `snapshot.rs` spawns what a snapshot
/// names: the id comes with the record, not from this peer's allocator.
fn spawn_labelled(world: &mut World, order: Order, label: u32, bundle: impl Bundle) {
    if matches!(order, Order::ForwardShiftedEntities) {
        for _ in 0..3 {
            let filler = world.spawn_empty().id();
            world.despawn(filler);
        }
        world.spawn(Name::new("filler"));
    }
    let id = TickTrackedEntity::new(SpawnerSlot::AUTHORITY, u64::from(label) + 1);
    world.spawn((Label(label), bundle, id));
    world.resource_mut::<TrackedIdAllocator>().raise_to(id);
}

/// A tick's worth of bodies, as `(label, bits)` sorted by label.
type Frame = Vec<(u32, Vec<u32>)>;

/// The first tick at which two traces differ, and which body.
fn first_divergence(a: &[Frame], b: &[Frame]) -> Option<(usize, u32)> {
    let at = a.iter().zip(b).position(|(x, y)| x != y)?;
    let body = a[at]
        .iter()
        .zip(&b[at])
        .find(|(p, q)| p != q)
        .map_or(u32::MAX, |(p, _)| p.0);
    Some((at, body))
}

/// Two leaning stacks side by side, so body-body contacts form and break all through the run.
fn placement(label: u32) -> (f32, f32, f32) {
    let column = (label % 2) as f32;
    let level = (label / 2) as f32;
    let lean = 0.18 * level;
    (column * 1.02 + lean, 0.5 + 1.05 * level, lean * 0.5)
}

/// A torso (label 0) and four limbs (labels 1..=4) hinged to it at its corners, the shape of
/// run-2d's ragdoll: every hinge shares the torso, so the order the solver visits them in is
/// the order their impulses compound in. Returns `(label, centre, anchor on torso, anchor on
/// limb)` for each limb.
fn limbs() -> [(u32, (f32, f32), (f32, f32), (f32, f32)); 4] {
    [
        (1, (-1.0, 1.0), (-0.5, 0.5), (0.5, -0.5)),
        (2, (1.0, 1.0), (0.5, 0.5), (-0.5, -0.5)),
        (3, (-1.0, -1.0), (-0.5, -0.5), (0.5, 0.5)),
        (4, (1.0, -1.0), (0.5, -0.5), (-0.5, 0.5)),
    ]
}

/// The hinges' labels, after the bodies'.
const HINGE_LABEL: u32 = 100;

#[cfg(feature = "3d")]
mod three_d {
    use super::*;
    use avian3d::prelude::*;
    use bevy_ticked_avian::avian3d::TickedAvianPlugin;
    use bevy_ticked_testing::fixtures::avian::install_with;

    fn spawn(world: &mut World, order: Order) {
        world.spawn((
            RigidBody::Static,
            Collider::cuboid(40.0, 1.0, 40.0),
            Transform::from_xyz(0.0, -0.5, 0.0),
        ));
        for label in permutation(order, BOXES) {
            let (x, y, z) = placement(label);
            spawn_labelled(
                world,
                order,
                label,
                (
                    RigidBody::Dynamic,
                    Collider::cuboid(1.0, 1.0, 1.0),
                    Transform::from_xyz(x, y, z),
                ),
            );
        }
    }

    fn sample(world: &mut World) -> Frame {
        let mut q = world.query::<(
            &Label,
            &Position,
            &Rotation,
            &LinearVelocity,
            &AngularVelocity,
        )>();
        let mut rows: Frame = q
            .iter(world)
            .map(|(l, p, r, v, w)| {
                let mut bits = Vec::new();
                bits.extend(p.0.to_array().map(f32::to_bits));
                bits.extend(r.0.to_array().map(f32::to_bits));
                bits.extend(v.0.to_array().map(f32::to_bits));
                bits.extend(w.0.to_array().map(f32::to_bits));
                (l.0, bits)
            })
            .collect();
        rows.sort();
        rows
    }

    fn trace(plugin: TickedAvianPlugin, order: Order) -> Vec<Frame> {
        let mut app = peer_app(HOST_UUID, move |app: &mut App| install_with(app, plugin));
        spawn(app.world_mut(), order);
        (0..TICKS)
            .map(|_| {
                app.update();
                sample(app.world_mut())
            })
            .collect()
    }

    fn check(plugin: TickedAvianPlugin, order: Order) -> Option<(usize, u32)> {
        let at = first_divergence(&trace(plugin, Order::Forward), &trace(plugin, order));
        println!("3d {order:?}: first divergence from Forward (tick, label): {at:?}");
        at
    }

    #[test]
    fn the_same_order_twice_is_bit_identical_3d() {
        assert_eq!(check(TickedAvianPlugin::default(), Order::Forward), None);
    }

    #[test]
    fn shifted_entity_indices_do_not_change_the_result_3d() {
        assert_eq!(
            check(TickedAvianPlugin::default(), Order::ForwardShiftedEntities),
            None
        );
    }

    #[test]
    fn reverse_spawn_order_does_not_change_the_result_3d() {
        assert_eq!(check(TickedAvianPlugin::default(), Order::Reverse), None);
    }

    #[test]
    fn interleaved_spawn_order_does_not_change_the_result_3d() {
        assert_eq!(
            check(TickedAvianPlugin::default(), Order::Interleaved),
            None
        );
    }

    /// Without the canonical order the reverse spawn parts at the first body-on-body contact,
    /// or the tests above prove nothing.
    #[test]
    fn without_the_canonical_order_spawn_order_changes_the_result_3d() {
        let plugin = TickedAvianPlugin::default().spawn_order_dependent_solve();
        assert!(check(plugin, Order::Reverse).is_some());
    }

    fn spawn_ragdoll(world: &mut World, order: Order) {
        world.spawn((
            RigidBody::Static,
            Collider::cuboid(40.0, 1.0, 40.0),
            Transform::from_xyz(0.0, -0.5, 0.0),
        ));
        let mut bodies = std::collections::HashMap::new();
        let body_order = permutation(order, 5);
        for &label in &body_order {
            let (x, y) = if label == 0 {
                (0.0, 0.0)
            } else {
                limbs()[label as usize - 1].1
            };
            spawn_labelled(
                world,
                order,
                label,
                (
                    RigidBody::Dynamic,
                    Collider::cuboid(0.9, 0.9, 0.9),
                    Position(Vec3::new(x, 4.0 + y, 0.2 * x)),
                    Rotation(Quat::from_rotation_z(0.3)),
                    AngularVelocity(Vec3::new(0.5, 0.0, 2.0)),
                ),
            );
            let entity = world
                .query::<(Entity, &Label)>()
                .iter(world)
                .find(|(_, l)| l.0 == label)
                .unwrap()
                .0;
            bodies.insert(label, entity);
        }
        for k in permutation(order, 4) {
            let (label, _, a1, a2) = limbs()[k as usize];
            let joint = RevoluteJoint::new(bodies[&0], bodies[&label])
                .with_local_anchor1(Vec3::new(a1.0, a1.1, 0.0))
                .with_local_anchor2(Vec3::new(a2.0, a2.1, 0.0))
                .with_angle_limits(-0.8, 0.8);
            spawn_labelled(world, order, HINGE_LABEL + label, joint);
        }
    }

    fn ragdoll_trace(plugin: TickedAvianPlugin, order: Order) -> Vec<Frame> {
        let mut app = peer_app(HOST_UUID, move |app: &mut App| install_with(app, plugin));
        spawn_ragdoll(app.world_mut(), order);
        (0..TICKS)
            .map(|_| {
                app.update();
                sample(app.world_mut())
            })
            .collect()
    }

    fn check_ragdoll(plugin: TickedAvianPlugin, order: Order) -> Option<(usize, u32)> {
        let at = first_divergence(
            &ragdoll_trace(plugin, Order::Forward),
            &ragdoll_trace(plugin, order),
        );
        println!("3d ragdoll {order:?}: first divergence from Forward (tick, label): {at:?}");
        at
    }

    #[test]
    fn the_same_ragdoll_twice_is_bit_identical_3d() {
        assert_eq!(
            check_ragdoll(TickedAvianPlugin::default(), Order::Forward),
            None
        );
    }

    #[test]
    fn a_ragdoll_spawned_in_reverse_is_bit_identical_3d() {
        assert_eq!(
            check_ragdoll(TickedAvianPlugin::default(), Order::Reverse),
            None
        );
    }

    #[test]
    fn a_ragdoll_spawned_interleaved_is_bit_identical_3d() {
        assert_eq!(
            check_ragdoll(TickedAvianPlugin::default(), Order::Interleaved),
            None
        );
    }

    /// Without the canonical order the hinges are solved in spawn order, and the reverse spawn
    /// parts in the first tick.
    #[test]
    fn without_the_canonical_order_a_ragdoll_depends_on_spawn_order_3d() {
        let plugin = TickedAvianPlugin::default().spawn_order_dependent_solve();
        assert!(check_ragdoll(plugin, Order::Reverse).is_some());
    }
}

#[cfg(feature = "2d")]
mod two_d {
    use super::*;
    use avian2d::prelude::*;
    use bevy_ticked_avian::avian2d::TickedAvianPlugin;

    fn install_with(app: &mut App, plugin: TickedAvianPlugin) {
        app.add_plugins((AssetPlugin::default(), bevy::scene::ScenePlugin))
            .init_asset::<Mesh>()
            .add_plugins(plugin)
            .insert_resource(Gravity(Vec2::NEG_Y * 9.81));
    }

    fn spawn(world: &mut World, order: Order) {
        world.spawn((
            RigidBody::Static,
            Collider::rectangle(40.0, 1.0),
            Transform::from_xyz(0.0, -0.5, 0.0),
        ));
        for label in permutation(order, BOXES) {
            let (x, y, _) = placement(label);
            spawn_labelled(
                world,
                order,
                label,
                (
                    RigidBody::Dynamic,
                    Collider::rectangle(1.0, 1.0),
                    Transform::from_xyz(x, y, 0.0),
                ),
            );
        }
    }

    fn sample(world: &mut World) -> Frame {
        let mut q = world.query::<(
            &Label,
            &Position,
            &Rotation,
            &LinearVelocity,
            &AngularVelocity,
        )>();
        let mut rows: Frame = q
            .iter(world)
            .map(|(l, p, r, v, w)| {
                let mut bits = Vec::new();
                bits.extend(p.0.to_array().map(f32::to_bits));
                bits.extend([r.cos.to_bits(), r.sin.to_bits()]);
                bits.extend(v.0.to_array().map(f32::to_bits));
                bits.push(w.0.to_bits());
                (l.0, bits)
            })
            .collect();
        rows.sort();
        rows
    }

    fn trace(plugin: TickedAvianPlugin, order: Order) -> Vec<Frame> {
        let mut app = peer_app(HOST_UUID, move |app: &mut App| install_with(app, plugin));
        spawn(app.world_mut(), order);
        (0..TICKS)
            .map(|_| {
                app.update();
                sample(app.world_mut())
            })
            .collect()
    }

    fn check(plugin: TickedAvianPlugin, order: Order) -> Option<(usize, u32)> {
        let at = first_divergence(&trace(plugin, Order::Forward), &trace(plugin, order));
        println!("2d {order:?}: first divergence from Forward (tick, label): {at:?}");
        at
    }

    #[test]
    fn the_same_order_twice_is_bit_identical_2d() {
        assert_eq!(check(TickedAvianPlugin::default(), Order::Forward), None);
    }

    #[test]
    fn shifted_entity_indices_do_not_change_the_result_2d() {
        assert_eq!(
            check(TickedAvianPlugin::default(), Order::ForwardShiftedEntities),
            None
        );
    }

    #[test]
    fn reverse_spawn_order_does_not_change_the_result_2d() {
        assert_eq!(check(TickedAvianPlugin::default(), Order::Reverse), None);
    }

    #[test]
    fn interleaved_spawn_order_does_not_change_the_result_2d() {
        assert_eq!(
            check(TickedAvianPlugin::default(), Order::Interleaved),
            None
        );
    }

    #[test]
    fn without_the_canonical_order_spawn_order_changes_the_result_2d() {
        let plugin = TickedAvianPlugin::default().spawn_order_dependent_solve();
        assert!(check(plugin, Order::Reverse).is_some());
    }

    fn spawn_ragdoll(world: &mut World, order: Order) {
        world.spawn((
            RigidBody::Static,
            Collider::rectangle(40.0, 1.0),
            Transform::from_xyz(0.0, -0.5, 0.0),
        ));
        let mut bodies = std::collections::HashMap::new();
        let body_order = permutation(order, 5);
        for &label in &body_order {
            let (x, y) = if label == 0 {
                (0.0, 0.0)
            } else {
                limbs()[label as usize - 1].1
            };
            spawn_labelled(
                world,
                order,
                label,
                (
                    RigidBody::Dynamic,
                    Collider::rectangle(0.9, 0.9),
                    Position(Vec2::new(x, 4.0 + y)),
                    Rotation::radians(0.3),
                    AngularVelocity(2.0),
                ),
            );
            let entity = world
                .query::<(Entity, &Label)>()
                .iter(world)
                .find(|(_, l)| l.0 == label)
                .unwrap()
                .0;
            bodies.insert(label, entity);
        }
        for k in permutation(order, 4) {
            let (label, _, a1, a2) = limbs()[k as usize];
            let joint = RevoluteJoint::new(bodies[&0], bodies[&label])
                .with_local_anchor1(Vec2::new(a1.0, a1.1))
                .with_local_anchor2(Vec2::new(a2.0, a2.1))
                .with_angle_limits(-0.8, 0.8);
            spawn_labelled(world, order, HINGE_LABEL + label, joint);
        }
    }

    fn ragdoll_trace(plugin: TickedAvianPlugin, order: Order) -> Vec<Frame> {
        let mut app = peer_app(HOST_UUID, move |app: &mut App| install_with(app, plugin));
        spawn_ragdoll(app.world_mut(), order);
        (0..TICKS)
            .map(|_| {
                app.update();
                sample(app.world_mut())
            })
            .collect()
    }

    fn check_ragdoll(plugin: TickedAvianPlugin, order: Order) -> Option<(usize, u32)> {
        let at = first_divergence(
            &ragdoll_trace(plugin, Order::Forward),
            &ragdoll_trace(plugin, order),
        );
        println!("2d ragdoll {order:?}: first divergence from Forward (tick, label): {at:?}");
        at
    }

    #[test]
    fn the_same_ragdoll_twice_is_bit_identical_2d() {
        assert_eq!(
            check_ragdoll(TickedAvianPlugin::default(), Order::Forward),
            None
        );
    }

    #[test]
    fn a_ragdoll_spawned_in_reverse_is_bit_identical_2d() {
        assert_eq!(
            check_ragdoll(TickedAvianPlugin::default(), Order::Reverse),
            None
        );
    }

    #[test]
    fn a_ragdoll_spawned_interleaved_is_bit_identical_2d() {
        assert_eq!(
            check_ragdoll(TickedAvianPlugin::default(), Order::Interleaved),
            None
        );
    }

    /// Without the canonical order the hinges are solved in spawn order, and the reverse spawn
    /// parts in the first tick.
    #[test]
    fn without_the_canonical_order_a_ragdoll_depends_on_spawn_order_2d() {
        let plugin = TickedAvianPlugin::default().spawn_order_dependent_solve();
        assert!(check_ragdoll(plugin, Order::Reverse).is_some());
    }
}
