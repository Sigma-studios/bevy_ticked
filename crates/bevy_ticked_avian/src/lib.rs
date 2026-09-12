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
//! nothing (measured in bevy_kart: two seconds of reverse at exactly zero speed).
//! `zero_warm_starting()` if a game has its own reason.
//!
//! **Sleeping** takes a body out of the solver once it has rested long enough. The sleep state
//! is not a registered component, so a rollback restores a body's position and velocity but
//! not whether the solver is skipping it: a body asleep on the client and awake on the host
//! diverges at the first replayed tick. Disabled by default (every `RigidBody` gets
//! `SleepingDisabled`, and `TimeToSleep` is infinite); `allow_sleeping()` for a solo game that
//! never rolls back.
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
//! the solve order), `PhysicsIslands` and each body's `BodyIslandNode`, and `JointGraph`.
//! Cloned once per tick into the history, restored with everything else, kept rather than
//! emptied when the session ends. Spawn and despawn bodies through the tracked paths
//! (`despawn_ticked`) so the entities a restored graph names still exist.
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
    ($avian:ident, $place_from:item) => {
    use $avian::collision::contact_types::ContactGraph;
    use $avian::dynamics::solver::SolverConfig;
    use $avian::dynamics::solver::constraint_graph::ConstraintGraph;
    use $avian::dynamics::solver::islands::{BodyIslandNode, PhysicsIslands};
    use $avian::dynamics::solver::joint_graph::JointGraph;
    use $avian::physics_transform::PhysicsTransformConfig;
    use $avian::prelude::*;
    use bevy::prelude::*;
    use bevy_ticked::TickedSimulation;
    use bevy_ticked::registry::{TickedAppExt, TickedComponentRegistry};
    use bevy_ticked::resource_registry::{TickedResourceAppExt, TickedResourceRegistry};
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
        /// Set `SolverConfig::warm_start_coefficient` to zero. Off by default: the contact
        /// graph is rolled back, impulses included, so warm starting replays cleanly.
        pub zero_warm_starting: bool,
        /// Let bodies sleep. A rollback cannot wake them; solo games only.
        pub allow_sleeping: bool,
        /// Do not add `PhysicsPlugins` even if none are present.
        pub physics_added_by_the_game: bool,
        /// Keep avian's `Transform` → `Position` sync on. Solo games only.
        pub positions_from_transforms: bool,
    }

    impl Default for TickedAvianPlugin {
        fn default() -> Self {
            Self {
                zero_warm_starting: false,
                allow_sleeping: false,
                physics_added_by_the_game: false,
                positions_from_transforms: false,
            }
        }
    }

    impl TickedAvianPlugin {
        /// Zero `SolverConfig::warm_start_coefficient` every run. Not needed for a replay to
        /// agree, and a body pressed into a wall is then held there; see the crate docs.
        pub fn zero_warm_starting(mut self) -> Self {
            self.zero_warm_starting = true;
            self
        }

        /// The default since warm starting was found replay-safe; kept so a game that opted
        /// in keeps compiling.
        pub fn keep_warm_starting(mut self) -> Self {
            self.zero_warm_starting = false;
            self
        }

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
    }

    impl Plugin for TickedAvianPlugin {
        fn build(&self, app: &mut App) {
            if !self.physics_added_by_the_game
                && !app.is_plugin_added::<$avian::schedule::PhysicsSchedulePlugin>()
            {
                app.add_plugins(PhysicsPlugins::new(TickedSimulation));
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
            register_resource_if_missing::<PhysicsIslands>(app);
            register_resource_if_missing::<JointGraph>(app);
            let node_registered = app
                .world()
                .get_resource::<TickedComponentRegistry>()
                .is_some_and(|registry| registry.index_of::<BodyIslandNode>().is_some());
            if !node_registered {
                app.register_ticked_component::<BodyIslandNode>();
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
        }

        fn finish(&self, app: &mut App) {
            assert!(
                app.is_plugin_added::<$avian::schedule::PhysicsSchedulePlugin>(),
                "TickedAvianPlugin: no PhysicsPlugins were added. Add \
                 `PhysicsPlugins::new(TickedSimulation)` before this plugin, or drop \
                 `.physics_added_by_the_game()`"
            );
            let world = app.world_mut();
            if self.zero_warm_starting {
                world
                    .get_resource_or_insert_with(SolverConfig::default)
                    .warm_start_coefficient = 0.0;
            }
            if !self.allow_sleeping {
                world.insert_resource(TimeToSleep(f32::INFINITY));
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
        fn place_from(transform: &Transform) -> (Position, Rotation) {
            (
                Position(transform.translation.truncate()),
                Rotation::radians(transform.rotation.to_scaled_axis().z),
            )
        }
    );
}
