# Migration

One section per phase of the netcode overhaul, in the order they landed. Each names what broke,
what to change in a game, and why. Both peers of a session must be built from the same commit.

Phases that changed `bevy_ensemble` too say which of its commits they pin; that crate's own
`docs/MIGRATION.md` covers what changed there.

## T6 — lockstep part 1: join, checksum, late actions, trust

One wire change: `ClientLoaded` carries the joiner's tick buffer. Both peers must be rebuilt;
`bevy_ensemble`'s protocol hash covers it, so a stale peer is refused at the handshake rather
than joined and hung.

### The join no longer depends on the two buffers agreeing (F23)

**Before** the host sized a joiner's grace window from `host_tick_buffer` alone, and the joiner
started its scheduled sequence wherever `client_tick_buffer` put it. When the joiner's buffer
was the larger — the adaptive tuner grows it on a bad link, and a client that had played on
satellite and then joined a LAN host carried it in — the joiner's first scheduled tick landed
after the first tick the host required of it, and the host waited for the ticks in between for
ever.

**After** `ClientLoaded { buffer }` tells the host what the joiner schedules with, and
`joined_at_tick = current + 1 + max(host_tick_buffer, joiner's buffer)`. On the joiner,
`LastScheduledTick` is set to the snapshot's tick once the game has applied it, so the first
flush fills forward from there; the host drops the batches for ticks it has already simulated.
When a lobby goes, `LockstepConfig` goes back to what the plugin was built with
(`InitialLockstepConfig` holds it) and `AdaptiveBufferState` is reset, so a buffer grown on one
link is not carried into a session on another.

**What to change** Nothing in a game that used `LockstepPlugin` as documented. A game that
constructed `ClientLoaded` itself gives it the buffer. A game that constructed `LockstepConfig`
by field adds `..default()` (it has a new field, below).

### The host has no input lag, and never waits on itself (F29)

**Before** the host scheduled its own actions `host_tick_buffer` ahead, as though it were a
client of itself, and its pause check required its own entry — which the flush that would have
inserted it does not run while held. With `host_tick_buffer: 0` that was a session that never
started.

**After** the host's actions go into the tick about to run; the authoritative broadcast carries
them to every client with that tick. `host_tick_buffer` is what it always effectively was: the
grace window a client's actions get after its `joined_at_tick`. It is clamped to at least one
at build, with a warning. The host's own participant is never in the set the pause check or the
broadcast waits on. A peer with no `LocalMultiplayerPlayerId` drops its local actions with a
`warn_once!` rather than filing them under uuid 0, a player no roster contains.
`AdaptiveBufferTuning::min_buffer` (default 4, never below 1) replaces the private constant.

**What to change** A game that read `host_tick_buffer` as "the host's input lag" reads it as
the grace window. A game that measured host-side input latency measures it again.

### The checksum log follows the session (F24)

**Before** a peer's `ChecksumLog` sampled whenever it ticked, lobby or not, and kept what it
had across a join. A player who idled in a menu and then joined had samples at tick numbers
the host was also at, of a different world, and the exchange reported a desync at the first tick
both logs shared.

**After** `ChecksumLog::clear()` (new, core crate) is called when a join snapshot arrives and
when the lobby goes; the exchange's parked reports go with it. `JoinSnapshotReceived` (local,
never on the wire) is the untyped message that carries "the world is about to be replaced" to
parts of the crate that do not know the game's snapshot type; the exchange reads it after
`LockstepJoinSet::ApplyJoinSnapshot` and before the finalize set flips the client to ready.
`bevy_ticked_lockstep_networking::ChecksumLogPlugin` is now the lockstep crate's own: the core
sampler gated on `With<Lobby>`, with the same `in_set`. It samples nothing while there is no
lobby unless built with `.sample_without_lobby()`.

```rust
// Before: the core plugin, via the lockstep crate's re-export
app.add_plugins(ChecksumLogPlugin::<MyHash>::default());
// After: the same line, now the lockstep sampler. To keep sampling solo:
app.add_plugins(ChecksumLogPlugin::<MyHash>::default().sample_without_lobby());
```

**What to change** A game that relied on solo samples — a determinism test comparing two solo
runs — adds `.sample_without_lobby()`, or names `bevy_ticked::checksum::ChecksumLogPlugin`
explicitly. A game that imports both preludes and names `ChecksumLogPlugin` unqualified now has
two: import the one it means. `bevy_ticked_testing`'s lockstep peers use the core plugin and are
unaffected.

### Late and far-future client actions (F25)

**Before** `receive_client_actions` merged whatever a client scheduled, for whatever tick. An
action for a tick the host had already simulated changed the host's record of it: the clients
had applied the tick without it, the next client to join was caught up *with* it from the
tracker, and the two disagreed from then on. This happens on every join — the joiner's first
flush now fills from its snapshot tick, and the host is past that by the time the batch lands.
An action for a tick a million ahead was a tracker entry kept for a million ticks. A client
that requested a snapshot and went quiet pinned the tracker's floor for the rest of the session.

**After** the host drops a batch for `tick <= current` and for `tick > current +
action_horizon`, with a `debug!` each. `LockstepConfig::action_horizon` is new, default 128.
`PendingClientJoins` forgets a uuid when its `LobbyClient` is removed
(`forget_departed_client_joins`, an observer on `Remove`) and when it has been pending longer
than `PENDING_JOIN_WINDOW_TICKS` (4096) — the tracker's keep-floor is bounded by that.

**What to change** `LockstepConfig { .. }` literals add `..default()`. A game that read
`PendingClientJoins` to keep its own catch-up state reads it knowing entries can leave.

### Trust (F27)

**Before** every message from a client was taken at face value.

**After**
- A `JoinSnapshotRequest` from a uuid that is already a participant is ignored; a repeat from
  the same uuid within `JOIN_SNAPSHOT_REQUEST_INTERVAL` (1 s, on the frame clock) is answered
  by the capture already under way. `LastJoinSnapshotRequests` is the bookkeeping.
- A `ClientLoaded` from a uuid that is already a participant is ignored: it used to re-issue
  `joined_at_tick`, which reopened the grace window in which the sender's missing actions
  count as empty. The three systems that read `ClientLoaded` off the wire now read
  `ClientAccepted` (local, never on the wire), written once per accepted join by
  `activate_loaded_client_participants`.
- `ClientScheduledActions` and `ChecksumReport` are accepted only from
  `LockstepLobbyParticipant`s of the local lobby.
- On the host a `ChecksumReport` latches `Desync` only against the host's own sample at that
  tick; a report for a tick the host has no sample for is dropped, not parked. A client still
  parks, because it is behind the host by construction.

**What to change** Nothing, unless a game sent one of these itself. A game whose clients sent
`ClientLoaded` more than once — some did, on every snapshot — sees the repeats logged at
`debug` and ignored.

### The roster is applied before the tick loop

`apply_received_participants` and `apply_pending_lockstep_participants` moved from `Update` to
`PreUpdate`, after `EnsembleSet::ReceivePackets`. A participant is on the roster before the
first tick of the frame its `ParticipantJoined` arrived in, which is what lets a game spawn the
player *inside the tick* at `joined_at_tick` and have every peer do it on the same tick. A game
that ordered against those systems in `Update` removes the ordering.

### `block_placer` (F28)

The example spawns players inside `TickedSimulation` from the roster's `joined_at_tick`
(`spawn_joined_players`), sorted by uuid, rather than from an `Update` observer on the frame
the roster arrived — which was a different tick on every peer. Sprites are attached in `Update`
by `attach_visuals` to the bodies the tick spawned bare. The join snapshot carries velocity;
one that carried only positions handed the joiner a body at rest where the host's was moving.
`ChecksumLogPlugin<GameHash>` (in `PhysicsSystems::Last`) and `ChecksumExchangePlugin` are
wired in, and a desync is an `error!` and a line in the UI. A departed player's body stays:
there is no tick every peer agrees the player left on until part 2 puts a leave on the tick
timeline, and despawning on the frame the roster changed would desync exactly as the old
spawn did.

### Tests

`crates/bevy_ticked_lockstep_networking/tests/{join_buffers,checksum_lifecycle,trust}.rs`, over
a shared `tests/common` fixture whose world is one counter. `testing::participant_joined_at` is
new; `testing::push_action` panics when the peer has no `LocalPendingActions<A>` (an integer
literal defaulting to `i32` on a `u8` peer used to push nothing and pass).

## T5 — `TickHolds`: one pause vocabulary

Nothing on the wire changed.

### `TicksPaused` is gone; `TickHolds` replaces it

**Before** one marker resource. A joining client inserted it until its first snapshot, a
lockstep peer inserted and removed it every frame from what it had received, a game inserted
it for the pause menu, and every `remove_resource::<TicksPaused>()` lifted everybody's. A
user's pause was undone by the first snapshot to arrive; a lockstep client's wait was undone
by the game's unpause.

**After** `TickHolds`, a set of `TickHoldReason`s: `Manual`, `AwaitingSync`, `SessionPause`,
`WaitingForPeers`, `SoftHold`, `Custom(u8)`. The clock advances when the set is empty. Each
subsystem holds and releases its own reason and never touches another's: `reset_on_join`
holds `AwaitingSync` and the first snapshot releases it; the lockstep sync sets
`WaitingForPeers`; `reset_on_leave` and `reset_on_host` release the session's reasons only.

```rust
// Before
commands.insert_resource(TicksPaused);         // pause
commands.remove_resource::<TicksPaused>();     // resume
if ticks_paused.is_some() { .. }               // read
// After
holds.hold(TickHoldReason::Manual);
holds.release(TickHoldReason::Manual);
if holds.is_held() { .. }                      // any reason
if holds.holds(TickHoldReason::Manual) { .. }  // this one
```

**Delete** a game's `toggle_ticks_paused` that re-inserted the marker every frame to fight
the stack, and any `remove_resource::<TicksPaused>()` on lobby join. **Watch** a UI that
showed "PAUSED" from the marker: `is_held()` says the clock is stopped, `reasons()` says why,
which is the difference between "paused" and "waiting for the host".

### A paused host keeps broadcasting

While its clock is held, a host still sends its snapshot, once every 32 passes of the loop
instead of every tick, so a client that joins during a pause receives the world. A client
receiving the same tick repeatedly drops the duplicates as stale.

`SnapshotApplied.first` now means "the client was holding `AwaitingSync`", which is what it
meant before under another name.

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
