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

## Host changes (T17, E5)

shooting_ropes picks this up on its next `cargo update` (ticked `main`, ensemble `master`). Over WebRTC
nothing migrates until the signalling server runs E5b. It adopts roles itself (`net/lobby.rs`) rather
than through `TickedEnsembleSessionPlugin`, so the bridge's end-and-re-adopt on `HostChanged` does not
run for it, and this step is the game's to write.

| Where | Today | On a host change | Covered by |
|---|---|---|---|
| `net/lobby.rs:191` `release_lobby` | runs when no lobby is left; despawns the tracked world, removes the roles, sets `Authority::Solo`, zeroes the tick | also run it on `HostChanged`, keeping `LocalMultiplayerPlayerId` (the lobby stands and the id is the same). Calling `bevy_ticked_networking_ensemble::end_ticked_session` does the bridge's share | `a_host_change_ends_the_snapshot_session_for_every_survivor` |
| `net/lobby.rs:129` `adopt_lobby` | runs only while `Authority::Solo` | nothing once the release above sets `Solo`: a promoted peer's lobby has `Host`, so it becomes `Authority::Host`. A follower should wait for `HandshakeVerified` on its lobby before adopting, as the bridge's `adopt_role` does, or its registry handshake can time out during the reconnect | `a_slow_reconnect_to_the_new_host_does_not_time_out_the_registry_handshake` |
| `menu/net/room.rs:240` `follow_the_host_into_the_world`, `app/state.rs:90` `end_the_world` | a client enters `InGame` when `WorldInfo` arrives | on `HostChanged`, go to `AppState::Menu` / `MenuScreen::Lobby`; the new host presses Start and picks the world, as for a fresh lobby | — |
| `saves/apply.rs:271` | saves on `OnExit(InGame)` | check it only writes a world this peer hosted: after a host change a follower leaves `InGame` holding the old host's world | — |
| `net/checksum.rs:168` `broadcast_checksum` | needs `Authority::Host` and a `(Lobby, Host)` | nothing, once `Authority` follows the promotion | — |
| lobby / in-game UI | — | show `AwaitingHost { waited, successor }`: "the host left, waiting for a new one" / "reaching the new host" | `a_client_that_loses_its_host_keeps_its_lobby_and_waits` (ensemble) |
| `menu/net/room.rs:198` Leave, `ui/settings.rs:415` | despawn the lobbies | a host's leave now hands the lobby to the earliest-joined player. Add an action that writes `CloseLobby` if the host should end it for everyone | `a_closed_lobby_ends_for_everyone_and_does_not_migrate` (ensemble) |
