# Architecture

Five crates, one clock.

```
bevy_ticked                the tick: TickedLoop, histories, rollback, lifetimes, checksum
  └─ bevy_ticked_networking     roles: server snapshots, client prediction/replay, deltas, pause, input
       ├─ bevy_ticked_networking_ensemble   transport bridge: handshake, session, welcome, overlay
       └─ bevy_ticked_avian                 avian under the tick, replay-safe by construction
  └─ bevy_ticked_lockstep_networking   the other model: everyone runs every input, on a ruled tick
bevy_ticked_testing        the harness the six games had each written, plus what none had
```

## `bevy_ticked` — the tick

`TickedPlugin { source, max_ticks_per_frame, history_ticks, simulation_executor }` owns a
clock (`TickSource::Hz`, `Manual`, or Bevy's `FixedUpdate` for solo scrubbing) and runs
`TickedLoop` once per tick: `TickedSystems::{Restore, PreTick, SampleInput, Tick, PostTick}`.
`Tick` runs the game's `TickedSimulation` schedule with `Time` swapped to the tick's clock,
single-threaded by default, then captures every registered component and resource into a
per-type history (`WorldActions<T>`, a ring of `HistoryBufferTicks` ticks).

Rollback is `restore_all(tick)`: components, resources, and *existence* —
`TrackedEntityLifetimes` records when each tracked id was born and died, `despawn_ticked`
tombstones (`Disabled`) rather than destroys, and a restore revives or tombstones so the world
at `tick` is the world that was. Ids are `sequence << 8 | slot`: the authority mints in slot
0, each client in the slot the host hands it, so predicted spawns never collide.
`TickHolds` is the one pause vocabulary: a set of reasons; ticks run when it is empty.
`checksum` hashes the world per tick for agreement checks; `diagnostics::TickCost` times
the tick.

## `bevy_ticked_networking` — client/server

Transport-free. `TickedServerPlugin<I>` broadcasts a `SnapshotPacket` per recipient every
`send_every` ticks: a full body (entity-major, sorted, components in wire-index order) or a
delta against the newest packet that client acknowledged (`delta.rs`), LZ4 over 256 bytes.
`TickedClientPlugin<I>` predicts ahead of the host by a measured lead, compares each snapshot
with what it predicted byte for byte (the fast path, no replay when they agree), and otherwise
restores the snapshot tick and replays, bounded per frame. Entities are `Interpolated` by
default (drawn from the authoritative history, never simulated) and `Predicted` for the
local player; `TickedSmoothingPlugin` slides corrections. `pause.rs` replicates a session
pause as a ticked resource; `input_plugin.rs` samples the local input once per tick.
`diagnostics.rs` counts everything the audit had to measure by hand.

The wire: `PROTOCOL_VERSION` 2, indices derived from sorted names, `wire_hash()` compared at
join. Every type is registered by name; there is no unnamed registration.

## `bevy_ticked_networking_ensemble` — the bridge

Maps the networking crate's observers onto `bevy_ensemble` messages and does what a session
needs done once: `TickedEnsembleSessionPlugin` adopts the roles from the lobby, runs the
registry handshake (names and hashes, refusing a mismatch by naming the first differing
registration), hands each client its spawner slot and the send rate in
`TickedSessionWelcome`, keeps `SnapshotRecipientList` to verified clients, and forwards
pause requests. `overlay.rs` publishes the counters to bevy_ensemble's net-debug overlay.
`local_session` (feature `local-session`) runs a whole local session from one shell:
`TICKED_LOCAL_SESSION=N cargo run` starts a signalling server in-process, launches `N - 1`
copies of the executable as clients and hosts.

## `bevy_ticked_avian`

`TickedAvianPlugin` per dimension: registrations under `avian::*`, sleeping disabled, `Transform` → `Position` sync off (bodies placed once from their spawn
transform), and avian's persistent solver state — contact graph, constraint graph, islands,
joint graph — rolled back with the bodies. `docs/avian.md` has what each setting was found to
cost.

## `bevy_ticked_lockstep_networking`

The host rules each tick's actions (`AuthoritativeTick { tick, players_actions, system,
margins }`); every peer runs the same actions on the same tick. Joins, leaves, pauses and
kicks are system actions on a ruled tick (`LockstepRoster`, `TickedEvents<RosterChange>`);
`StallPolicy` names who is waited on and kicks after a bound; a joiner catches up at up to
1.5×; buffers are sized from measured arrival margins. `checksum_exchange` compares hashes
across peers and latches `Desync` on the host's confirmation.

## `bevy_ticked_testing`

`TickedNetwork` runs N peers over `bevy_ensemble_loopback` in one process with a link that
can be told to misbehave; `view`, `assert`, `measure`, `fault`, `golden`, `source_guard` and
`fixtures` are the vocabulary the tests in every crate are written in. Feature `avian` adds
the physics fixture. Every crate has a `tests/sim_is_deterministic.rs` source guard.

## Where things are decided

| Question | Answer, and where |
|---|---|
| What does the wire look like | `registry.rs` (`frozen()`), `snapshot.rs`, `delta.rs` |
| When does a client replay | `client.rs::handle_server_snapshot` (`prediction_matches`) |
| When is an entity alive | `lifetimes.rs` |
| Who may pause | `pause.rs` (`PausePolicy`) |
| What a game must obey | `docs/ROLLBACK_RULES.md` |
| What changed, per phase | `docs/MIGRATION.md`; per game, `docs/migration/` |
