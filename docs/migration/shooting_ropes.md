# shooting_ropes

A 2D rope-swinging shooter, client/server, with the most thorough `docs/upstream-needs.md`
(1379 lines) of the six; §3.1 (`TickedSessionLobby`) and §3.12 (ids never reissued) are
upstream behaviours now, with the tests named after the sections.

| Step | Delete | Replaced by | Covered by |
|---|---|---|---|
| 2 | `net/registry.rs` index assertions, `NetIndex` | derived indices (T7) | `derived_indices_are_stable_under_reordering` |
| 2 | `WorldChecksum.wire`, the game's `measure_prediction` | `bevy_ticked::checksum`, `measure_prediction::<T>` (T3, T8) | `checksum_lifecycle.rs` |
| 3 | `assert_tick_delta`, `HistoryBufferTicks` | T4 | `tick_clock.rs`, `history_window.rs` |
| 4 | `net/lobby.rs::{adopt_lobby, release_lobby}` (212 lines with the rest) | `TickedEnsembleSessionPlugin`, `TickedSessionLobby(Entity)` (T7) | `handshake` tests |
| 5 | `net/authority.rs` (115 lines): `Authority::decides`, `may_spawn_tracked`, `spawn_tracked`, `LocalPlayerUuid` | `TrackedSpawner::spawn_by`, `LocalSpawnerSlot` (T10) | `a_client_never_mints_an_id_the_host_will_reuse` |
| 6 | `net/smoothing.rs` (149 lines) | `TickedSmoothingPlugin` (T8) | `a_small_correction_decays_and_never_snaps` |
| 7 | the input enqueue step in `net/input.rs` | `TickedInputPlugin` (T14) | `a_frame_that_runs_two_ticks_samples_twice` |
| 9 | `net/probe.rs`, `net/rollback_debug.rs` counters | `ReplayStats`, `TickCost`, `HealthWarnings` (T3) | `diagnostics.rs` |

**Keep:** `net/reconstruct.rs`'s idea — a rope re-dressed by an `On<Add, TickTrackedEntity>`
observer — is exactly how a rebuilt entity comes back after a rewind past a plain despawn;
the observer stays, `despawn_ticked` makes it rarely needed.
