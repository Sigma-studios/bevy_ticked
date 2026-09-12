# avian under the tick

What `bevy_ticked_avian::TickedAvianPlugin` sets, why, and what the tests measured. The
short version: add the plugin after `TickedPlugin` and `PhysicsPlugins::new(TickedSimulation)`,
place bodies by `Position`, put your systems in `TickedSimulationSet`, and a stack of boxes
replays bit-identically from any tick in its history.

## The bundle

| Setting | Value | Why |
|---|---|---|
| `Position`, `Rotation`, `LinearVelocity`, `AngularVelocity` | networked, names `avian::*` | The body's state on the wire, under names both peers share. |
| `SolverConfig` | untouched | Warm starting seeds the solver with the previous step's impulses, and those are in the rolled-back `ContactGraph`, so a replay seeds from the same ones. The bundle used to zero it, which held a body driven into a wall in place for seconds. |
| Sleeping | off (`SleepingDisabled` required on every `RigidBody`, `TimeToSleep` infinite) | The sleep state is not restored by a rollback; a body asleep here and awake there diverges at the first replayed tick. |
| `PhysicsTransformConfig::transform_to_position` | `false` | avian would adopt a blended or camera-moved `Transform` as the body's place. A body spawned with a `Transform` is placed once, when its `RigidBody` is added. |
| `ContactGraph`, `ConstraintGraph`, `PhysicsIslands`, `JointGraph` | rollback-only ticked resources, kept on leave | The solver reads last step's manifolds and the constraint colouring before it reads any body. Without them the replay diverged in every body at its first tick. |
| `BodyIslandNode` | rollback-only ticked component | Agrees with `PhysicsIslands`. |
| `TickedSimulationSet::{Input, BeforePhysics, Physics, AfterPhysics}` | chained; avian's sets inside `Physics` | Where a game's systems go. |

Opt-outs, each for a solo game that never rolls back: `allow_sleeping()`,
`positions_from_transforms()`. `physics_added_by_the_game()` when
`PhysicsPlugins` carry a length unit or collision hooks.

## What the tests found

`crates/bevy_ticked_avian/tests/determinism.rs`, on a ground and six leaning unit cubes that
topple and tumble:

- **`a_stack_of_boxes_replays_bit_identically_with_the_documented_bundle`**: 96 ticks
  replayed from tick 40, every bit of every body's position, rotation and velocities equal
  to the live run, with every tracked `Transform` poisoned before the replay.
- **`sleeping_is_disabled_for_tracked_bodies_or_the_replay_diverges`**: with
  `allow_sleeping()`, the first body sleeps at tick 218 and a replay across it diverges at
  its first tick; the bundle replays the same window cleanly.
- **`the_avian_stack_golden_matches`**: the trace sampled every eight ticks for four seconds
  matches `tests/golden/avian_stack.trace`. Re-record with `UPDATE_GOLDEN=1` and read the
  diff.
- **Warm starting**: the first test passes with it on and with it zeroed; the contact graph
  is what carries the impulses across a rollback. It was zeroed until bevy_kart measured what
  that costs against stacked wall contacts: the kart driven into a wall was held there, two
  seconds of reverse at exactly zero speed, where the same build with warm starting pulled
  away at once, tick for tick like the build before the bundle.

The search that led to the contact graph: restoring `SolverBody`, the pre-solve deltas, the
AABBs, `Transform`, `GlobalTransform` — each per-body cache avian keeps — changed nothing;
restoring `ContactGraph` alone made the next tick bit-identical, and the constraint graph and
islands then had to agree with it or the solver panicked on a contact id it no longer had.

## Rules for a game

1. Place bodies by `Position` and `Rotation`, or by `Transform` at spawn only.
2. Drive bodies from `InputQueue` in `TickedSimulationSet::Input`; read contacts and
   positions in `AfterPhysics`.
3. Spawn and despawn through the tracked paths (`TrackedSpawner`, `despawn_ticked`): a
   restored contact graph names entities, and they must still exist.
4. Register every component your systems write (`register_ticked_component` at least); the
   `assert_replays_identically::<YourHash>` check in `bevy_ticked_testing` finds the ones
   you missed, the way it found the contact graph.
5. Kinematic bodies for interpolated remote entities on clients, so the solver does not
   fight the drawn position.
