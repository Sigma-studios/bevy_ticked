# Migration

One section per phase of the netcode overhaul, in the order they landed. Each names what broke,
what to change in a game, and why. Both peers of a session must be built from the same commit.

Phases that changed `bevy_ensemble` too say which of its commits they pin; that crate's own
`docs/MIGRATION.md` covers what changed there.

## T4 — core correctness, no wire change

Nothing on the wire changed. Every peer should still be rebuilt: the client's rollback now
restores more, and a client from before this phase disagrees with one from after about what a
correction leaves behind.

### The blend never reaches the simulation (`TickedSystems::Restore`)

`TickedInterpolationPlugin` writes a blended `Transform` for the renderer. It used to stay in
the component into the next tick, which then integrated from a transform `fraction` of the way
back toward the previous tick: a body meant to cross 99 units in 99 ticks crossed 75, and the
games that measured it read it as "physics feels floaty under Hz" and pinned the tick source
to `FixedUpdate`. `TickedSystems::Restore` is a new first set in `TickedLoop`; the plugin puts
the true transform (and `GlobalTransform`, for entities without a parent) back there before
anything reads it.

**Delete** the game's own `transform_to_position: false` workaround if it was only there for
this, and the `FixedUpdate` pin. **Watch** anything else that writes a presentation value into
a simulated component between ticks: put its undo in `TickedSystems::Restore`.
`TickedInterpolation::current()` is new and returns the simulation's value.

### Networked apps refuse `TickSource::FixedUpdate`

`TickedClientPlugin`, `TickedServerPlugin` and `LockstepPlugin` panic in `finish` when
`TickedPlugin` was built with `FixedUpdate`, with a message that says what to change. A
client steers its prediction lead by running a couple of percent fast or slow, and Bevy's
fixed clock cannot be stretched; on `FixedUpdate` it nudged by a whole tick instead, which is
a visible jump in everything the simulation draws. `TickedPlugin::default()` keeps
`FixedUpdate` for solo play and scrubbing; `TickSource::Manual` is still accepted (a test
harness paces it).

```rust
// Before
app.add_plugins(TickedPlugin::default());
// After
app.add_plugins(TickedPlugin { source: TickSource::Hz(64.0), ..default() })
   .add_plugins(TickedInterpolationPlugin);
```

`ConfiguredTickSource` (resource), `TickSource::is_steerable()` and
`require_steerable_tick_source(app, "MyPlugin")` are new for plugins with the same need.

### A client's correction restores everything, not only what travelled

`handle_server_snapshot` now restores rollback-only components and resources (registered
without a wire name) to their value at the snapshot's tick before replaying, and truncates
the `TickedEvents` logs after it. Before, a rollback-only component kept the client's last
predicted value into the replay, and an event from a tick the authority erased stayed
presented. `TickedComponentRegistry::restore_local_only` is the new entry point.

### The frame clocks read the tick inside the simulation

`run_tick_schedule` swaps `Time<Virtual>`, `Time<Fixed>` and `Time<Real>` to the tick's
delta and elapsed for the duration of the tick, and puts them back. A timer or tween that
read `Time<Virtual>` inside a tick used to get the frame's delta on the first run and the
tick's on a replay. The source guard still flags the read; `Time` is the one to use.

### `TickedSimulation` runs single-threaded

`TickedPlugin::simulation_executor` defaults to `SimulationExecutor::SingleThreaded`. Two
unordered systems that write the same component ran in an order that differed per frame and
per peer. A game with every ordering explicit can set `MultiThreaded` back.

### History is sized by whoever knows

`TickedPlugin::history_ticks: Option<u64>`. `None` (the default) installs the hundred-second
scrubbing window and lets a role plugin replace it: `TickedClientPlugin` installs `128` ticks,
which is every tick a snapshot can still name. `Some(n)` is the game's choice and is never
replaced (`HistoryWindowChosen` marks it). **Delete** a game's `HistoryBufferTicks(256)`
unless it scrubs.

### Manual steps run the whole loop

`StepForward` runs `TickedLoop` with `StepOnce` present, so `PreTick` and `PostTick` run on
a step (the interpolation pair shifts, a host broadcasts, a checksum samples); `StepBackward`
and `ResetToTick` run the loop with `RestoredThisPass` present, so everything sees the
restored world and the tick does not advance. Scrubbing is no longer a different simulation
from playing.

### Examples

The networked examples build with `Hz(64.0)` and integrate from `Time::delta_secs()`;
`SECONDS_PER_TICK` is no longer read inside a tick anywhere in this repository. Do the same:
inside a tick `Time` is the tick clock, on the first run and on every replay.

See also `docs/ROLLBACK_RULES.md`, new, for the rules a simulation has to obey and the clippy
configuration that catches a plain `despawn`.

### The host's input window (F8)

`collect_network_inputs` now refuses an input whose tick is outside
`[server_tick - HistoryBufferTicks, server_tick + MAX_INPUT_LEAD_TICKS]`. A refused input is
counted in `InputStats.dropped_out_of_window` (which was reserved in T3 and is live now) and
then ignored entirely: it does not enter the queue, does not move the sender's margin, and
does not become the sender's newest tick. Before this one client could file a tick at
`u64::MAX`, and every snapshot then carried a nine-quintillion margin that no lead could shed.

`bevy_ticked_networking::input::MAX_INPUT_LEAD_TICKS` (64) is the upper edge. It **must equal**
the client's `ClientTickBuffer::MAX_TICKS`, which is now spelled in terms of it; a game that
raised the client's ceiling by hand must raise this one too, or its clients are refused at the
new ceiling. The lower edge is the prune window, because an older tick is unreplayable anyway.

**New message: `messages::PeerLeft(pub u128)`.** A transport writes it on the host when a
client leaves, and the server forgets that uuid's queued inputs at every tick, its
`InputMargins` entry and its `NewestInputTick` mark. `TickedEnsembleSessionPlugin` writes it
when a `LobbyClient` is removed, so a game on the ensemble bridge does nothing. A game on its
own transport triggers it itself (`commands.trigger(PeerLeft(uuid))`); without it a departed
player's body keeps obeying its last keypress until the window prunes it, and a rejoin under
the same uuid finds all of its inputs older than "newest".

### `InputQueue` is ordered (F31, part)

`InputQueue<T>::inputs` is `BTreeMap<u64, BTreeMap<u128, T>>` (was a `HashMap` of `HashMap`s)
and `at_tick` returns `Option<&BTreeMap<u128, T>>`. A simulation that folded over "every
player's input this tick" saw a different order on every peer, because `RandomState` is seeded
per process, and diverged wherever the order mattered. Code that only calls `get`, `insert`,
`at_tick(..).get(&uuid)` or iterates `for (uuid, input) in inputs` compiles unchanged; code
that named the `HashMap` type must say `BTreeMap`. Two additions: `players()` (every uuid in
the queue, once, ascending) and `remove_player(uuid)`.

### Capture allocates nothing (F9, part)

`WorldActions<T>` keeps its history in a `VecDeque` in tick order and recycles the emptied
per-tick maps through a pool; `capture_component` takes one back out with `take_map()` and
caches its query. After warm-up a capture allocates zero bytes
(`tests/allocation_free_capture.rs` counts them). `WorldActions::history` was `pub(crate)`
and is private now; every reader goes through `at_tick`, `recorded_ticks`, `recorded_range`,
`oldest_recorded_tick`, `newest_recorded_tick`. Nothing on the public surface changed.

### `TickedResource: Default`, and `reset_all` (F35)

`TickedResource` (and so `NetworkedTickedResource`) now requires `Default`. A registered
resource is session state by definition, and session state has to have a value that means
"no session". **Add `#[derive(Default)]`** to every registered resource; a type with no
sensible default was never session state and belongs on a tracked entity.

`TickedResourceRegistry::reset_all(&self, world)` puts every registered resource that is
present back to `R::default()` and clears its history. `reset_on_leave` and `reset_on_join`
call it. **Delete** a game's `reset_round_on_leave`-style system: the `RoundState` that walked
into the next lobby still saying "round 7" is what this closes. A registered resource that is
*absent* stays absent; absence is a state the game chose.

## T3 — the testing crate and the numbers a session shows

Pins `bevy_ensemble` at `3ba5229` (E0 loopback harness, E1 trust and liveness, E2 protocol v2,
E3 ICE restart). Nothing on bevy_ticked's own wire changed, but E2 changed the transport's, so
every peer must be rebuilt.

### `bevy_ticked_testing`, a dev-dependency for every game

New crate at `crates/bevy_ticked_testing`. It is the harness the six games each wrote a copy
of, upstream: in-process peers over `bevy_ensemble_loopback` with lossy links, a view of any
peer's world, input scripts, convergence and determinism assertions, a source guard, a wire
guard, goldens, and fault injection. Add it under `[dev-dependencies]` and delete the local
copy; the per-game checklists under `docs/migration/` name which files.

```toml
[dev-dependencies]
bevy_ticked_testing = { git = "https://github.com/Sigma-studios/bevy_ticked" }
# features: "lockstep" for the lockstep peers, "avian" for the avian fixture
```

Its crate docs are the reference. Two things it does that the game copies did not: a peer's
frame rate is a cadence on the network's clock (`set_ticks_per_frame(peer, 2.0)` is a 32 fps
peer that updates every other frame, not a peer whose clock runs twice as fast), and every
assertion has a test in `tests/bites.rs` that shows it failing. The shape of a test:

```rust
use bevy_ticked_testing::prelude::*;

let mut net = TickedNetwork::client_server(host_app, [client_app]);
net.set_link_all(Link::wifi());
net.settle();
net.hold_input(client, Input { forward: true }, 64);
assert_converged::<Pos>(&net, host, client, 8);
```

### Named ensemble registrations

E2 requires every `register_ensemble_message_type` call to carry a wire name. The bridge's own
types are named `bevy_ticked/Snapshot`, `bevy_ticked/Input`, `bevy_ticked/RegistryHandshake`;
lockstep's `bevy_ticked_lockstep/*`. A game registering its own ensemble messages must name
them; see `bevy_ensemble/docs/MIGRATION.md` E2.

### `LOCAL_PLAYER_UUID` is gone

E1 deleted bevy_ensemble's placeholder identity. `bevy_ticked_networking_ensemble` no longer
compares against it; a game that did should delete the comparison. The bridge only adopts a
role once the transport has published a real `LocalMultiplayerPlayerId`.

### `bevy_ticked::checksum` (was `bevy_ticked_lockstep_networking::checksum`)

`WorldHash`, `ChecksumLog`, `ChecksumLogPlugin` and `Divergence` moved into the core crate so a
client-server game can hash its world the same way a lockstep one does. The lockstep crate
re-exports the module, so `bevy_ticked_lockstep_networking::checksum::WorldHash` still resolves;
prefer `bevy_ticked::prelude::*`.

### Diagnostics, always compiled

Every game wrote counters. They are upstream now, counted where the event happens, so a test
and an overlay read the same number:

| Resource | Crate | Inserted by | What it counts |
|---|---|---|---|
| `TickCost` | `bevy_ticked::diagnostics` | `TickedPlugin` | ticks run (replays included), time spent, worst tick |
| `ReplayStats` | `bevy_ticked_networking::diagnostics` | `TickedClientPlugin` | snapshots applied, rollbacks, ticks replayed, last replay distance, stale and pre-role drops |
| `SnapshotStats` | same | `TickedServerPlugin` | broadcasts, bytes (per recipient, filled by the bridge), max and last size |
| `InputStats` | same | `TickedServerPlugin` | inputs received, late |
| `HealthWarnings` | same | `TickedClientPlugin` | client-minted tracked ids, snapshots older than history; each warns once then counts |

**Delete** a game's own `SimRuns`-style counter bracketing `TickedLoop`, its snapshot byte
counter at the bridge, its `debug_assert!` that a client never advanced
`TickTrackedEntityCounter`, and its "measure the frame time" overlay line: `TickCost` measures
the tick, which is the thing this crate runs. Replays count as ticks on purpose.

**Overlay** With the `overlay` feature of `bevy_ticked_networking_ensemble` (default on, pulls
`bevy_ensemble/netdebug`), the bridge publishes `ticked.tick`, `ticked.rate`, `ticked.replay`,
`ticked.snapshot`, `ticked.input` and `ticked.health` lines to `NetDebugExtras`. A game that
published its own under those keys should stop.

**Watch** `ReplayStats.skipped_identical` and `InputStats.dropped_out_of_window` stay zero
until T9 and T4 land; they exist now so a test written today keeps its shape.

### Tests that came upstream with it

`crates/bevy_ticked_networking_ensemble/tests/{lossy_links,alt_tab,determinism}.rs` are
run-2d's network tests over the harness fixture: every link preset, jitter, duplication,
reordering, asymmetric links, three clients, mixed frame rates, a step change in latency, a
ten-thousand-tick session, and both alt-tab cases. `a_host_alt_tab_auto_pauses_and_no_lead_piles_up`
is ignored until the pause phase; today the client runs 132 ticks ahead of a host that
produced none. Every crate has `tests/sim_is_deterministic.rs`, a source guard against
frame-clock and input reads inside the simulation, with dated exceptions.

Two findings the determinism test records rather than hides: the snapshot is a `HashMap` on
the wire, so two encodings of one world can differ in byte order (fixed by the wire phase), and
the client seeds its lead from the ping round trip, which over loopback is the wall clock, so
a trace is reproducible in fates and outcome but not byte for byte.

## T0 — one pin for bevy_ensemble

`bevy_ensemble` and its backends are pinned once, by commit, in the workspace manifest, and
each crate takes them with `workspace = true`. A game pins the same commit its bevy_ticked
revision names, or the two disagree about the transport's wire format. `PLAN.md` is a pointer;
the plan it held described crates that no longer exist.
