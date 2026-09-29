//! avian under `bevy_ticked`, configured so a replayed tick is the tick it replays.
//!
//! Every game that put avian on the tick wrote the same five lines and forgot one of them:
//! register the four body components under stable names, run physics inside
//! `TickedSimulation`, roll the solver's own state back, keep bodies from sleeping, and order
//! the game's systems around the physics step. [`TickedAvianPlugin`] is those five lines, with
//! the ones that matter for determinism on by default and an opt-out each.
//!
//! # Why each setting
//!
//! **Warm starting** seeds the solver with the previous step's contact impulses. Those impulses
//! live in the contact graph, which this plugin rolls back with the bodies, so a replayed tick
//! seeds from the same impulses the first run did and warm starting is left on: the stack of
//! boxes replays bit-identically either way (`tests/determinism.rs`). It used to be zeroed,
//! and a body driven into a wall under a constant force was then held there for seconds --
//! the solver never accumulated the impulse to separate stacked contacts, and reversing did
//! nothing (measured in bevy_kart: two seconds of reverse at exactly zero speed). The plugin
//! does not touch `SolverConfig` at all; a game that wants another solver setting writes it.
//!
//! **Sleeping** takes a body out of the solver once it has rested long enough. The sleep state
//! is not a registered component, so a rollback restores a body's position and velocity but
//! not whether the solver is skipping it: a body asleep on the client and awake on the host
//! diverges at the first replayed tick. Disabled by default (every `RigidBody` gets
//! `SleepingDisabled`, and `TimeToSleep` is infinite); `allow_sleeping()` for a solo game that
//! never rolls back.
//!
//! **Islands** go with sleeping. avian groups touching bodies into islands so it can put a
//! whole resting group to sleep at once, and with sleeping off that is all the bookkeeping is
//! for. Under rollback it was also wrong: avian attaches a body's `BodyIslandNode` through
//! deferred observers (on spawn, and again when a tombstone is revived), the history restores
//! the historic one directly, and a body that came out of that dance without a node panicked the
//! next time it touched anything — `Neither body A nor B is in an island`, in `merge_islands`,
//! seen in run-2d every few rounds. So with sleeping off the plugin leaves avian's `IslandPlugin`
//! and `IslandSleepingPlugin` out: there is no `PhysicsIslands` resource, the narrow phase skips
//! the merge, and nothing is rolled back that avian is not also maintaining. A game that adds
//! `PhysicsPlugins` itself has to leave them out the same way (`.build().disable::<IslandPlugin>()
//! .disable::<IslandSleepingPlugin>()`), and `finish` refuses to start if it did not.
//!
//! **Transform → Position** is off. avian copies a changed `Transform` into `Position` before
//! each step; the renderer's blended transform, or one the game moved for a camera, would be
//! adopted by the simulation as the body's place (the audit's probe: 75 units in 99 ticks at
//! 64 units a second). A body is placed by `Position` and `Rotation`; the plugin initialises
//! both from `Transform` once, when a `RigidBody` is added with a non-identity `Transform`
//! and default `Position`/`Rotation`, so `Transform::from_xyz` at spawn keeps working for root
//! entities. `positions_from_transforms()` turns the sync back on for a solo game that moves
//! bodies through `Transform` and never rolls back.
//!
//! **The solver's own state is rolled back.** avian keeps every contact manifold between
//! steps — anchors, feature ids, the impulses of the previous step — and the solver reads it
//! before it reads a body's position. A replay that restored positions and velocities but
//! not the manifolds diverged at its first tick, in every body, in the fourth decimal
//! (`tests/determinism.rs` found it; no per-body component was enough). The plugin
//! registers `ContactGraph` as a rollback-only ticked resource, and with it the state that
//! must agree with it: `ConstraintGraph` (which contacts are constraints, in which colour —
//! the solve order) and `JointGraph`, plus `PhysicsIslands` and each body's `BodyIslandNode`
//! when sleeping is allowed and islands therefore exist. Cloned once per tick into the history,
//! restored with everything else, kept rather than emptied when the session ends. Spawn and
//! despawn bodies through the tracked paths (`despawn_ticked`) so the entities a restored graph
//! names still exist.
//!
//! **The solve order is named by tracked ids, not by spawn order.** avian's result depends on
//! the order its colliders and joints were spawned in: the broad phase names a new pair's
//! colliders in the order their proxies entered the collider trees, the narrow phase computes the
//! manifold in that orientation, the constraint graph colours contacts greedily in the order they
//! started touching (and the colours are solved in sequence), and joints are solved in the order
//! a query visits their table rows. A client never builds its world in the host's order -- a
//! snapshot spawns in id order, a rollback revives tombstones in map order, the host's tables are
//! spawn order shuffled by every despawn -- so without this no prediction of a contact or a
//! ragdoll could ever match the host's bit for bit (`tests/spawn_order_independence.rs`: the
//! same eight boxes spawned in reverse part at the first body-on-body contact). The plugin flips
//! each new contact pair so the collider with the lower tracked id is first, rebuilds the
//! constraint colouring every step in tracked-id order, and re-seats joint rows in tracked-id
//! order whenever a spawn or rollback has disturbed them. A collider without its own
//! `TickTrackedEntity` is keyed by its body's, and an untracked one (level geometry) by its
//! `Entity`, which is canonical only if every peer spawns it the same way.
//! `spawn_order_dependent_solve()` turns all of it off.
//!
//! # Sets
//!
//! [`TickedSimulationSet`] orders a game's systems around the step inside `TickedSimulation`:
//! `Input` (read the queue, set velocities and forces), `BeforePhysics`, `Physics` (avian's own
//! sets, all of them), `AfterPhysics` (read positions, resolve hits, spawn from contacts).
//!
//! # Setup
//!
//! ```ignore
//! app.add_plugins(TickedPlugin { source: TickSource::Hz(64.0), ..default() })
//!    .add_plugins(TickedAvianPlugin::default())
//!    .insert_resource(Gravity(Vec3::NEG_Y * 9.81))
//!    .add_systems(TickedSimulation, apply_inputs.in_set(TickedSimulationSet::Input));
//! ```
//!
//! The plugin adds `PhysicsPlugins::new(TickedSimulation)` unless the game added its own
//! (`.with_length_unit`, collision hooks) before it. Add it *after* `TickedPlugin`.
//!
//! One plugin per dimension: [`avian3d::TickedAvianPlugin`] (feature `3d`) and
//! [`avian2d::TickedAvianPlugin`] (feature `2d`); a workspace that uses both enables both.

#[cfg(not(any(feature = "2d", feature = "3d")))]
compile_error!("bevy_ticked_avian: enable the `2d` feature, the `3d` feature, or both");

use bevy::prelude::*;

/// Where a game's systems go, around the physics step, inside `TickedSimulation`.
#[derive(SystemSet, Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TickedSimulationSet {
    /// Read the input queue; set velocities, forces, controller targets.
    Input,
    /// Anything else that has to happen before the step.
    BeforePhysics,
    /// avian's own sets, first to last.
    Physics,
    /// Read positions and contacts; resolve hits; spawn and despawn from them.
    AfterPhysics,
}

macro_rules! ticked_avian {
    ($avian:ident, [$($joint:ty),* $(,)?], $place_from:item) => {
    use $avian::collision::contact_types::{ContactGraph, ContactId};
    use $avian::dynamics::solver::constraint_graph::ConstraintGraph;
    use $avian::dynamics::solver::islands::{
        BodyIslandNode, IslandPlugin, IslandSleepingPlugin, PhysicsIslands,
    };
    use $avian::dynamics::joints::EntityConstraint;
    use $avian::dynamics::solver::joint_graph::JointGraph;
    use bevy::ecs::storage::TableId;
    use $avian::physics_transform::PhysicsTransformConfig;
    use $avian::prelude::*;
    use bevy::prelude::*;
    use bevy_ticked::TickedSimulation;
    use bevy_ticked::registry::{TickedAppExt, TickedComponentRegistry};
    use bevy_ticked::resource_registry::{TickedResourceAppExt, TickedResourceRegistry};
    use bevy_ticked::tracked_entity::TickTrackedEntity;
    use bevy_ticked_networking::networked_registry::NetworkedTickedAppExt;

    pub use crate::TickedSimulationSet;

    /// The wire names the four body components are registered under.
    pub const POSITION_WIRE_NAME: &str = "avian::Position";
    pub const ROTATION_WIRE_NAME: &str = "avian::Rotation";
    pub const LINEAR_VELOCITY_WIRE_NAME: &str = "avian::LinearVelocity";
    pub const ANGULAR_VELOCITY_WIRE_NAME: &str = "avian::AngularVelocity";

    /// avian on the tick, replay-safe by default. See the crate docs.
    #[derive(Clone, Copy, Debug)]
    pub struct TickedAvianPlugin {
        /// Let bodies sleep, and keep avian's islands, which exist to sleep them. A rollback
        /// cannot wake a sleeping body; solo games only.
        pub allow_sleeping: bool,
        /// Do not add `PhysicsPlugins` even if none are present.
        pub physics_added_by_the_game: bool,
        /// Keep avian's `Transform` → `Position` sync on. Solo games only.
        pub positions_from_transforms: bool,
        /// Orient contact pairs, colour contact constraints and order joints by tracked id rather
        /// than by spawn order. On by default; see the crate docs.
        pub canonical_solve_order: bool,
    }

    impl Default for TickedAvianPlugin {
        fn default() -> Self {
            Self {
                allow_sleeping: false,
                physics_added_by_the_game: false,
                positions_from_transforms: false,
                canonical_solve_order: true,
            }
        }
    }

    impl TickedAvianPlugin {
        pub fn allow_sleeping(mut self) -> Self {
            self.allow_sleeping = true;
            self
        }

        /// The game adds `PhysicsPlugins::new(TickedSimulation)` itself, before this plugin.
        pub fn physics_added_by_the_game(mut self) -> Self {
            self.physics_added_by_the_game = true;
            self
        }

        /// Keep avian reading `Transform` into `Position` every step. A rollback then depends on
        /// nothing having touched `Transform` between ticks; solo games only.
        pub fn positions_from_transforms(mut self) -> Self {
            self.positions_from_transforms = true;
            self
        }

        /// Leave contacts and joints in avian's own order, which follows spawn order. Two peers
        /// that spawned the same bodies in different orders then solve every contact and every
        /// joint differently. For a game that never sends a snapshot.
        pub fn spawn_order_dependent_solve(mut self) -> Self {
            self.canonical_solve_order = false;
            self
        }
    }

    impl Plugin for TickedAvianPlugin {
        fn build(&self, app: &mut App) {
            if !self.physics_added_by_the_game
                && !app.is_plugin_added::<$avian::schedule::PhysicsSchedulePlugin>()
            {
                if self.allow_sleeping {
                    app.add_plugins(PhysicsPlugins::new(TickedSimulation));
                } else {
                    // No islands: see the crate docs. Without the plugin there is no
                    // `PhysicsIslands`, and the narrow phase never merges one.
                    app.add_plugins(
                        PhysicsPlugins::new(TickedSimulation)
                            .build()
                            .disable::<IslandPlugin>()
                            .disable::<IslandSleepingPlugin>(),
                    );
                }
            }

            register_if_missing::<Position>(app, POSITION_WIRE_NAME);
            register_if_missing::<Rotation>(app, ROTATION_WIRE_NAME);
            register_if_missing::<LinearVelocity>(app, LINEAR_VELOCITY_WIRE_NAME);
            register_if_missing::<AngularVelocity>(app, ANGULAR_VELOCITY_WIRE_NAME);
            // The solver's persistent state, rolled back with the bodies. Kept on leave: avian
            // keeps these consistent with the colliders that still stand, and an empty graph
            // under standing bodies is what would break it.
            register_resource_if_missing::<ContactGraph>(app);
            register_resource_if_missing::<ConstraintGraph>(app);
            register_resource_if_missing::<JointGraph>(app);
            // Islands only exist when sleeping does. Registering them without the plugin would
            // be harmless (a missing resource is skipped) but would document a promise the
            // bundle no longer makes.
            if self.allow_sleeping {
                register_resource_if_missing::<PhysicsIslands>(app);
                let node_registered = app
                    .world()
                    .get_resource::<TickedComponentRegistry>()
                    .is_some_and(|registry| registry.index_of::<BodyIslandNode>().is_some());
                if !node_registered {
                    app.register_ticked_component::<BodyIslandNode>();
                }
            }

            if !self.positions_from_transforms {
                app.world_mut()
                    .get_resource_or_insert_with(PhysicsTransformConfig::default)
                    .transform_to_position = false;
                app.add_observer(place_from_transform_once);
            }

            app.configure_sets(
                TickedSimulation,
                (
                    TickedSimulationSet::Input,
                    TickedSimulationSet::BeforePhysics,
                    TickedSimulationSet::Physics,
                    TickedSimulationSet::AfterPhysics,
                )
                    .chain(),
            )
            .configure_sets(
                TickedSimulation,
                (
                    PhysicsSystems::First,
                    PhysicsSystems::Prepare,
                    PhysicsSystems::StepSimulation,
                    PhysicsSystems::Writeback,
                    PhysicsSystems::Last,
                )
                    .in_set(TickedSimulationSet::Physics),
            );

            if !self.allow_sleeping {
                app.register_required_components::<RigidBody, SleepingDisabled>();
            }

            if self.canonical_solve_order {
                app.add_systems(
                    PhysicsSchedule,
                    (
                        orient_new_contact_pairs.in_set(BroadPhaseSystems::Last),
                        (recolor_constraint_graph, $(reseat_joints::<$joint>,)*)
                            .chain()
                            .after(PhysicsStepSystems::NarrowPhase)
                            .before(PhysicsStepSystems::Solver),
                    ),
                );
            }
        }

        fn finish(&self, app: &mut App) {
            assert!(
                app.is_plugin_added::<$avian::schedule::PhysicsSchedulePlugin>(),
                "TickedAvianPlugin: no PhysicsPlugins were added. Add \
                 `PhysicsPlugins::new(TickedSimulation)` before this plugin, or drop \
                 `.physics_added_by_the_game()`"
            );
            if !self.allow_sleeping {
                app.world_mut().insert_resource(TimeToSleep(f32::INFINITY));
                // Loud at startup rather than a panic in `merge_islands` a few rounds in: a
                // game that added `PhysicsPlugins` itself brought the islands with them.
                assert!(
                    !app.is_plugin_added::<IslandPlugin>(),
                    "TickedAvianPlugin: avian's IslandPlugin is added and sleeping is off. \
                     Islands exist to sleep bodies, and under rollback their bookkeeping \
                     panics (`Neither body A nor B is in an island`). Add \
                     `PhysicsPlugins::new(TickedSimulation).build()\
                     .disable::<IslandPlugin>().disable::<IslandSleepingPlugin>()` instead, \
                     or `allow_sleeping()` for a solo game that never rolls back"
                );
            }
        }
    }

    /// With the per-step sync off, a body spawned with a `Transform` and nothing else would start
    /// at the origin: place it once, when its `RigidBody` arrives with the transform.
    fn place_from_transform_once(
        add: On<Add, RigidBody>,
        bodies: Query<(&Transform, &Position, &Rotation)>,
        mut commands: Commands,
    ) {
        let Ok((transform, position, rotation)) = bodies.get(add.entity) else {
            return;
        };
        let placed = *position != Position::default() || *rotation != Rotation::default();
        if placed || *transform == Transform::IDENTITY {
            return;
        }
        commands
            .entity(add.entity)
            .insert(place_from(transform));
    }

    /// Where a collider sits in the canonical order: its own tracked id, else its body's tracked
    /// id (a child collider), else its `Entity` (level geometry spawned the same way on every
    /// peer). Tracked before untracked.
    ///
    /// **Sibling child colliders are the one spawn-dependent case.** Two untracked colliders on
    /// one tracked body share its id, and the tie between them is broken by `Entity`, which is
    /// whatever the spawn order made it. A body whose several colliders can touch the same thing
    /// in one step — a ragdoll limb with two capsules, a car with a collider per wheel — orders
    /// those contacts by spawn order again. Give each such child collider its own
    /// `TickTrackedEntity` (spawn it through `TrackedSpawner`) and the tie is gone.
    type ContactKey = (u8, u64, u64);

    fn contact_key(
        collider: Entity,
        tracked: &Query<(Option<&TickTrackedEntity>, Option<&ColliderOf>)>,
        bodies: &Query<&TickTrackedEntity>,
    ) -> ContactKey {
        match tracked.get(collider) {
            Ok((Some(id), _)) => (0, id.0, 0),
            Ok((None, Some(of))) => match bodies.get(of.body) {
                Ok(id) => (1, id.0, collider.to_bits()),
                Err(_) => (2, collider.to_bits(), 0),
            },
            _ => (2, collider.to_bits(), 0),
        }
    }

    /// The broad phase names a new pair's colliders in the order it found them, which follows
    /// the order their proxies entered the collider trees: spawn order. Everything the narrow
    /// phase computes for the pair (normal, anchors, feature ids) and the solver's view of it
    /// follows that orientation. Flip every pair that has no contact yet so the collider with
    /// the lower [`ContactKey`] is `collider1`. A pair with manifolds is left alone: it was
    /// oriented here when it was new, and flipping it would orphan its manifolds.
    fn orient_new_contact_pairs(
        mut contact_graph: ResMut<ContactGraph>,
        tracked: Query<(Option<&TickTrackedEntity>, Option<&ColliderOf>)>,
        bodies: Query<&TickTrackedEntity>,
        mut flip: Local<Vec<ContactId>>,
    ) {
        flip.clear();
        for pair in contact_graph.active_pairs() {
            if !pair.manifolds.is_empty() || pair.flags.contains(ContactPairFlags::TOUCHING) {
                continue;
            }
            if contact_key(pair.collider2, &tracked, &bodies)
                < contact_key(pair.collider1, &tracked, &bodies)
            {
                flip.push(pair.contact_id);
            }
        }
        for &id in flip.iter() {
            let Some((edge, pair)) = contact_graph.get_mut_by_id(id) else {
                continue;
            };
            core::mem::swap(&mut edge.collider1, &mut edge.collider2);
            core::mem::swap(&mut edge.body1, &mut edge.body2);
            core::mem::swap(&mut pair.collider1, &mut pair.collider2);
            core::mem::swap(&mut pair.body1, &mut pair.body2);
        }
    }

    /// avian colours contact constraints greedily, in the order the narrow phase reports the
    /// pairs that started touching (contact-id order, which is insertion order, which is spawn
    /// order), and solves the colours in sequence: the colour a constraint lands in decides
    /// which impulses it sees. Rebuild the colouring every step from scratch, pushing the
    /// manifolds in [`ContactKey`] order, so the solve order is a function of which pairs touch
    /// and nothing else.
    fn recolor_constraint_graph(
        mut contact_graph: ResMut<ContactGraph>,
        mut constraint_graph: ResMut<ConstraintGraph>,
        tracked: Query<(Option<&TickTrackedEntity>, Option<&ColliderOf>)>,
        bodies: Query<&TickTrackedEntity>,
        mut handles: Local<Vec<ContactId>>,
        mut order: Local<Vec<(ContactKey, ContactKey, ContactId, usize)>>,
    ) {
        handles.clear();
        for color in &constraint_graph.colors {
            handles.extend(color.manifold_handles.iter().map(|h| h.contact_id));
        }
        if handles.is_empty() {
            return;
        }
        handles.sort_unstable_by_key(|id| id.0);
        order.clear();
        for run in handles.chunk_by(|a, b| a == b) {
            let id = run[0];
            let Some((_, pair)) = contact_graph.get_by_id(id) else {
                continue;
            };
            order.push((
                contact_key(pair.collider1, &tracked, &bodies),
                contact_key(pair.collider2, &tracked, &bodies),
                id,
                run.len(),
            ));
        }
        // Stable, and keyed on the pair: two pairs never share both keys, so the order is total.
        order.sort_by_key(|&(k1, k2, _, _)| (k1, k2));

        constraint_graph.clear();
        for &(_, _, id, manifolds) in order.iter() {
            let Some((edge, pair)) = contact_graph.get_mut_by_id(id) else {
                continue;
            };
            debug_assert_eq!(manifolds, pair.manifolds.len());
            edge.constraint_handles.clear();
            for _ in 0..manifolds {
                constraint_graph.push_manifold(edge, pair);
            }
        }
    }

    /// A joint's place in the canonical order: its own tracked id, else its bodies' — and, for
    /// two untracked joints on the same pair of bodies, the joint's own `Entity`, so that the key
    /// is total and the order it gives is the same from one step to the next. That last tie is
    /// spawn-dependent across peers in the way `ContactKey`'s sibling case is: track such joints.
    type JointKey = (u8, u64, u64, u64);

    /// Set on a joint for the instant it is moved out of its table and back. See
    /// [`reseat_joints`].
    #[derive(Component)]
    struct Reseated;

    /// avian solves joints the way it prepares, warm-starts and damps them: by iterating a
    /// query over the joint components, which visits rows in table order -- spawn order, shuffled
    /// by every `swap_remove`. Every joint of a ragdoll shares the torso, so the order the solver
    /// visits them in is the order their corrections compound in.
    ///
    /// Nothing in avian takes an order, so the rows are put in one: when the joints of a type are
    /// out of [`JointKey`] order within any archetype, every one of them is moved out of its
    /// table (a marker inserted) and back (removed), in key order. A move appends, so each table
    /// is left holding them sorted. Costs one pass over the joints each step, and the moves only
    /// in a step whose order a spawn, revive or rollback disturbed. Order is checked per *table*,
    /// because that is what a query iterates: two archetypes can share a table (they differ only
    /// in sparse-set components), and their rows interleave in it. Tables are still visited in
    /// the order the world created them, which a peer that built its world differently may not
    /// share; joints of one type that all carry the same components live in one table.
    fn reseat_joints<C: Component + EntityConstraint<2>>(
        world: &mut World,
        joints: &mut QueryState<
            (Entity, &C, Option<&TickTrackedEntity>),
            (Without<RigidBody>, Without<JointDisabled>),
        >,
        mut rows: Local<Vec<(TableId, JointKey, Entity)>>,
    ) {
        rows.clear();
        for (entity, joint, id) in joints.iter(world) {
            let key = match id {
                Some(id) => (0, id.0, 0, 0),
                None => {
                    let body = |e: Entity| {
                        world.get::<TickTrackedEntity>(e).map_or(e.to_bits(), |t| t.0)
                    };
                    let [a, b] = joint.entities();
                    (1, body(a), body(b), entity.to_bits())
                }
            };
            let table = world.entity(entity).location().table_id;
            rows.push((table, key, entity));
        }
        let in_order = rows
            .windows(2)
            .all(|w| w[0].0 != w[1].0 || w[0].1 <= w[1].1);
        if in_order {
            return;
        }
        rows.sort_by_key(|&(_, key, _)| key);
        for &(_, _, entity) in rows.iter() {
            world.entity_mut(entity).insert(Reseated);
        }
        for &(_, _, entity) in rows.iter() {
            world.entity_mut(entity).remove::<Reseated>();
        }
    }

    fn register_resource_if_missing<R: bevy_ticked::resource_registry::TickedResource>(
        app: &mut App,
    ) {
        let registered = app
            .world()
            .get_resource::<TickedResourceRegistry>()
            .is_some_and(|registry| registry.index_of::<R>().is_some());
        if !registered {
            app.register_ticked_resource_kept_on_leave::<R>();
        }
    }

    fn register_if_missing<
        T: bevy_ticked_networking::networked_registry::NetworkedTickedComponent,
    >(
        app: &mut App,
        wire_name: &'static str,
    ) {
        let registered = app
            .world()
            .get_resource::<TickedComponentRegistry>()
            .is_some_and(|registry| registry.index_of::<T>().is_some());
        if !registered {
            app.register_networked_ticked_component::<T>(wire_name);
        }
    }

        $place_from
    };
}

/// avian3d under the tick. Feature `3d`.
#[cfg(feature = "3d")]
pub mod avian3d {
    ticked_avian!(
        avian3d,
        [
            FixedJoint,
            RevoluteJoint,
            SphericalJoint,
            PrismaticJoint,
            DistanceJoint
        ],
        fn place_from(transform: &Transform) -> (Position, Rotation) {
            (
                Position(transform.translation),
                Rotation(transform.rotation),
            )
        }
    );
}

/// avian2d under the tick. Feature `2d`.
#[cfg(feature = "2d")]
pub mod avian2d {
    ticked_avian!(
        avian2d,
        [FixedJoint, RevoluteJoint, PrismaticJoint, DistanceJoint],
        fn place_from(transform: &Transform) -> (Position, Rotation) {
            (
                Position(transform.translation.truncate()),
                Rotation::radians(transform.rotation.to_scaled_axis().z),
            )
        }
    );
}
