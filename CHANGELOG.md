# Changelog

The netcode overhaul, one entry per phase, newest first. `docs/MIGRATION.md` has the detail;
pull requests #1–#14 on this repository are the phases.

## Unreleased

- **T16** — `bevy_ticked_avian` keeps warm starting: the rolled-back contact graph carries its
  impulses, and zeroing it held a kart driven into a wall in place. `zero_warm_starting()` opts in.
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
