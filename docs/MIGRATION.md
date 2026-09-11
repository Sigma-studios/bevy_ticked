# Migration

One section per phase of the netcode overhaul, in the order they landed. Each names what broke,
what to change in a game, and why. Both peers of a session must be built from the same commit.

Phases that changed `bevy_ensemble` too say which of its commits they pin; that crate's own
`docs/MIGRATION.md` covers what changed there.

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
