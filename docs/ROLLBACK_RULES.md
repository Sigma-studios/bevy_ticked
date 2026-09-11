# What a simulation has to obey to roll back

A rollback re-runs ticks. Everything a tick does has to produce the same result the second
time, on this peer and on every other. These are the rules, each with the way it was broken
in a shipped game and the tool that now catches it.

## 1. Read the tick clock, not the frame clock

Inside `TickedSimulation`, `Time` **is** the tick clock: `delta()` is one tick, `elapsed()` is
`tick × timestep`, on the first run and on every replay. Since T4 the frame clocks
(`Time<Virtual>`, `Time<Fixed>`, `Time<Real>`) are swapped to the same values for the
duration of the tick, so a system that reached for one by habit no longer diverges. It is
still a wrong question: the source guard (`tests/sim_is_deterministic.rs`, from
`bevy_ticked_testing::source_guard`) flags `Time<Virtual>`, `Time<Real>`, `Instant::now`,
`SystemTime` and `SECONDS_PER_TICK` in simulation code. Integrate from `Time::delta_secs()`.

*How it was broken:* every example integrated by the `SECONDS_PER_TICK` constant while the
tick clock could be set to another rate, and nothing reported the mismatch.

## 2. Read input from the queue, not the keyboard

`ButtonInput<KeyCode>`, `Gamepads`, `CursorMoved`, `AccumulatedMouseMotion` inside a tick
read *this frame's* input; a replayed tick reads a different frame's. Sample input once per
tick outside the simulation, stamp it with the tick it will run in, and read
`InputQueue::at_tick` inside. The source guard flags the direct reads.

## 3. No wall-clock randomness

`rand::rng()`, `thread_rng`, `fastrand::` with no seed, `random::<T>()`: each replay draws
different numbers. Keep the RNG state in a ticked resource and seed it from the tick or a
networked seed. The source guard flags the global reaches.

## 4. Iterate in a fixed order

`HashMap` and `HashSet` from `std` iterate in a different order per process. A system that
folds over players' inputs, resolves collisions in query order, or spawns from a set walks
them in an order the other peer does not share. `InputQueue::at_tick` returns a `BTreeMap`
since T4; sort anything else by tracked id before acting on it. `TickedSimulation` runs on a
single-threaded executor by default (`TickedPlugin::simulation_executor`) so two unordered
systems cannot race.

## 5. Every simulated component is registered

A component the simulation writes and nobody registers is not rolled back: after a
correction it holds the last predicted value while everything around it is the authority's.
Register with `register_ticked_component` (rollback only) or
`register_networked_ticked_component` (also on the wire). `HealthWarnings` and the
`assert_replays_identically` harness assertion catch the ones that were missed.

## 6. Spawn and despawn through the tracked paths

Until the lifecycle phase lands, rewinding past a spawn leaves the entity standing with its
state stripped (the crate warns about "husks"), and rewinding past a despawn does not bring
it back. Spawn from the authority and let the snapshot replicate; use `despawn_ticked` once
it exists.

## Catching a plain `despawn` (clippy)

Add to the game's `clippy.toml`:

```toml
disallowed-methods = [
    { path = "bevy::ecs::system::EntityCommands::despawn", reason = "use despawn_ticked() on tracked entities so the rollback can resurrect them" },
    { path = "bevy::ecs::world::EntityWorldMut::despawn", reason = "use despawn_ticked() on tracked entities so the rollback can resurrect them" },
    { path = "rand::rng", reason = "a tick may be replayed; draw from a ticked RNG resource" },
    { path = "std::time::Instant::now", reason = "inside a tick, read Time; outside, prefer Time<Real>" },
]
```

and allow it explicitly (`#[allow(clippy::disallowed_methods)]`) at the few call sites that
despawn something the simulation does not track.
