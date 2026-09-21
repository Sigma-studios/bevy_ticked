# Changelog

The netcode overhaul, one entry per phase, newest first. `docs/MIGRATION.md` has the detail;
pull requests #1–#14 on this repository are the phases.

## Unreleased

- **Fix** — a tracked id handed to a different thing now starts from nothing. `lifetimes::reset`
  strips a reused entity back to what this crate owns — components *and* children — before the
  replay's bundle goes on, because an insert overwrites the types the new bundle names and says
  nothing about the rest. A pellet's id handed to a piece of a ragdoll used to arrive carrying the
  pellet's state, its sprite, its mesh and its children, and `redress` then fired the game's
  observer against a hybrid of two things. The snapshot path gets it too: `apply_full_body` revives
  a tombstone the authority still has, and since the marker never left, `Add<TickTrackedEntity>`
  did not fire there at all — so a peer that only *heard* about a reused id was never told to
  redress it, and kept the old occupant's picture while carrying the new one's state. Without
  this, every consumer needed its own hand-written list of "components some other kind of thing
  might have left here", wrong from the moment anybody adds a component and silent when it is.

- **Fix** — a peer that still holds an id **alive** is now told when it has been handed to
  something else. That was the third path and the only one with no local sign of it: a tombstoned
  id learns from the record that revives it, and a peer's own spawns go through `SpawnedAs`, but a
  peer holding the id alive as the previous occupant just gets components decoded onto it —
  `TickTrackedEntity` never left, so `Add` never fired, and the entity kept the old occupant's
  sprite and, in a physics game, its collider while carrying the new one's state. It is reachable:
  in a shooter, a client predicting a shot on the tick the authority instead resolved that player's
  death, since a player's own projectiles and their own ragdoll pieces mint under the same slot,
  and the ids collide inside a single tick so no snapshot ever omits the id first. Bodies now carry
  `reborn`, the ids handed over since the world that recipient acknowledged, which the authority
  already knows because it is what calls `reset`; `apply_full_body` resets and redresses each.
  Per recipient and baseline-relative, since a needless reset re-dresses an entity that was right.
  Comparing the record's type mask against the entity's shape is *not* an option — see `SpawnedAs`.
  Pinned by `id_changes_hands.rs`, which holds all three paths apart and checks that an id nobody
  named is left alone. **Wire change**: `FullBody` and `DeltaBody` gain a field.

- **Breaking** — `ReplicationClass::Once` and `register_networked_ticked_component_once` are gone;
  use `register_networked_ticked_component`. "With the entity's first record, never again" is once
  per **id**, and an id is not a thing — a replay hands a dead pellet's id to something else, and
  the recipient was never told the new kind or owner. This crate shipped the bug in its own
  `Owner` registration, the test fixtures taught it, and a consumer game had it too. The saving was
  a byte comparison: `Changed` already sends nothing when the encoded bytes match the baseline, so
  immutable data costs exactly the same on the wire and is now correct when it turns out not to be
  immutable after all. Migration is deleting `_once` from the call. It held only until
  some participant existed, and the host's announcement of the joiner itself (or of another
  joiner) could arrive before the roster it sends on acceptance: the joiner ran those ticks with
  nobody on `LockstepRoster`, so a game spawning from the roster was missing the host's bodies
  for a tick or two, and a checksum sampled then reported a desync. A client now also waits for
  its host to be on the roster.

- **Pin** — `bevy_ensemble` at `b71691a`: the WebRTC client (E5d) and Steam (E5e) turn host
  migration on, so T17 and T18 run against real transports. The per-game checklists gain a
  host-change section each.

- **T18** — a lockstep match survives its host. Survivors report the rulings they hold, the new
  host resumes from the furthest it can assemble without a gap (bounded by
  `HostMigrationPolicy::trust_window`) and rules on; nobody rewinds, and the old host leaves on
  the first tick the new one rules. New wire types: rebuild every peer.

- **T17** — a snapshot session survives its lobby changing host. `HostChanged` ends the ticked
  session on every peer and the roles are taken back in the same lobby once the new host is
  verified. `end_ticked_session` is public; `TickedNetwork` can lose and name hosts. Pins
  `bevy_ensemble` at `9cb1854` (host migration, E5a–E5c).

- **Fix** — an idle client's lead no longer runs away. A snapshot's `your_margin` is
  `MARGIN_UNMEASURED` when the host has timed no input from that client in the last
  `MARGIN_STALE_TICKS` (a quarter second), instead of zero; the client leaves its target alone
  on it. A client that sent nothing used to read the zero as "on time", set its target two
  ticks past its lead on every snapshot, and climb at the trim's full rate to the 64-tick
  ceiling in under a minute, where every snapshot cost a second of replay and four frames of
  holding. `InputMargins` now carries the tick each margin was measured at.
- **Fix** — a client no longer freezes when its host goes quiet. `client_soft_hold_after`
  defaults to `None`: at 250 ms it stopped the whole client, local player included, with
  nothing on screen, on every quarter-second hiccup of a link or a host frame (run-2d, "the
  game freezes for half a second, on the clients"). The client now runs through the silence,
  and the lead it piles up against a stalled host — the ticks the host never produced — is
  given back in one rewind when the host returns, once an excess of eight ticks has held for
  two applied snapshots (`SNAP_BACK_TICKS`, `SNAP_BACK_STREAK`), instead of shed at two
  percent a second with a replay that deep on every snapshot in between. Two readings have to
  agree before it fires — the replay distance and the host's margin report, which is what
  tells a host that stood still (inputs arriving early by the excess) from a link that slowed
  (inputs arriving late) — and for one round trip after it the margin reports are ignored,
  since they still describe the lead just given back. `ReplayStats` counts rewinds as
  `snapped_back`; a jittery link never triggers one, and a satellite-sized latency step
  settles in two.
- **Fix** — `bevy_ticked_avian` leaves avian's islands out when sleeping is off. Islands exist
  to sleep bodies, and under rollback their bookkeeping panicked: avian attaches a body's
  `BodyIslandNode` through deferred observers, the history restored the historic one directly,
  and a body left without a node died on its next contact with `Neither body A nor B is in an
  island` (run-2d, every few rounds). The bundle now adds `PhysicsPlugins` without
  `IslandPlugin` and `IslandSleepingPlugin`, registers `PhysicsIslands` and `BodyIslandNode`
  only under `allow_sleeping()`, and refuses at `finish` a game that added the islands itself.
- **Fix** — `run_tick_schedule` times the tick with `bevy::platform`'s clock rather than
  `std`'s. `std::time::Instant::now()` is a panicking stub on `wasm32-unknown-unknown`, and
  every tick goes through this function, so a web build died on its first one. The crate now
  asks `bevy` for `web` itself instead of inheriting it from whatever the consumer enabled.
- **T16** — `bevy_ticked_avian` keeps warm starting: the rolled-back contact graph carries its
  impulses, and zeroing it held a kart driven into a wall in place. The knob is gone.
- **T15** — documentation consolidation, `netpeer` examples, multi-process WebRTC tests,
  `scripts/netpeers.sh`, CI.
- **T14** — `bevy_ticked_avian` (replay-safe avian: registrations, warm start off, sleeping
  off, transform sync off, solver state rolled back); `TickedInputPlugin` and
  `TickedSystems::SampleInput`; `LocalPlayer`.
- **T13** — delta replication against acknowledged baselines, `nack_full`, replicate-once
  (`ReplicationClass`), `SendRates`, LZ4. 21.6 bytes a tick at rest, 47.5 for two walking
  players (audit: 1340).
- **T12** — lockstep part 2: system actions on a ruled tick, `LockstepRoster`,
  `StallPolicy` with kicks, joiner catch-up, margin-sized buffers.
- **T11** — the replicated session pause: `SessionPause`, `PausePolicy`, auto-pause on
  focus loss and real-time gaps, client soft hold.
- **T10** — existence history, `despawn_ticked` tombstones, slot-partitioned ids,
  `TrackedSpawner`, predicted spawns confirmed under the same id.
- **T9** — the misprediction fast path (no replay when the snapshot agrees),
  `send_every`, bounded replay per frame.
- **T8** — `ReplicationMode::{Predicted, Interpolated}`, `Owner`, hold-last input, relayed
  inputs, `TickedSmoothingPlugin`.
- **T7** — snapshot wire v2: names derive indices, entity-major bodies, per-recipient
  packets with `seq` and `your_margin`, the registry handshake.
- **T6** — lockstep part 1: joins that never deadlock, checksum lifecycle, late-action
  window, trust.
- **T5** — `TickHolds`, one pause vocabulary.
- **T4** — `TickedSystems::Restore`, networking refuses `FixedUpdate`, ordered input queue,
  frame clocks read the tick, single-threaded simulation, `TickedResource: Default`.
- **T3** — `bevy_ticked_testing`, `bevy_ticked::checksum`, diagnostics.
- **T0** — one pin for `bevy_ensemble`.
