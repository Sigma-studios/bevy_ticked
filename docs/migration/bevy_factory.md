# bevy_factory

A lockstep factory builder over WebRTC, with the in-process harness
(`src/testing/harness.rs`, 1002 lines) that became `bevy_ticked_testing`, the `netpeer` binary
and `tests/webrtc_multiprocess.rs` that became the `netpeer` examples and
`scripts/netpeers.sh` here.

| Step | Delete | Replaced by | Covered by |
|---|---|---|---|
| 1 | the generic parts of `src/testing/harness.rs`: peers, links, stepping, `step_frame_with_ticks` plumbing | `TickedNetwork::lockstep`, `freeze`, `step_frame_with_ticks` (E0, T3) | `bites.rs` |
| 1 | `tests/{lockstep_join, lockstep_liveness, buffer_tuning, tick_frame_alignment}.rs`, the generic halves | `crates/bevy_ticked_lockstep_networking/tests/{join_buffers, part2}.rs` (T6, T12) | `a_join_with_mismatched_buffers_does_not_deadlock`, `uneven_frame_pacing_between_peers_keeps_them_in_agreement` |
| 1 | `src/bin/netpeer.rs` scaffolding (arguments, checksum log, signalling server) | the `netpeer` example pattern and `scripts/netpeers.sh` (T15) | `webrtc_multiprocess.rs` |
| 2 | `LocalPlayerIdentity` + the placeholder sync | `Option<Res<LocalMultiplayerPlayerId>>` (E1) | `no_identity_is_published_until_the_backend_has_one` |
| 4 | `toggle_ticks_paused` | `TickHolds::hold(Manual)`/`release` (T5) | `a_user_pause_is_not_lifted_by_an_arriving_authoritative_tick` |
| 4 | `return_to_menu_when_client_lobby_is_lost` | `LobbyLeft { reason }` (E1) | ensemble liveness tests |
| 5 | the roster derivation in `spawn_players_at_agreed_tick` | `TickedEvents<RosterChange>` written on the ruled tick, `LockstepSimulationSet::Game` (T12) | `a_kicked_participant_leaves_on_the_same_tick_everywhere` |
| 9 | the game's `ChecksumLog` capacity dance | `ChecksumLogPlugin` (T3) | `checksum_lifecycle.rs` |

**Watch:** a joiner now catches up at up to 1.5× speed instead of the host freezing; a UI that
showed "waiting for player" during a join should read `LockstepStall` instead.

## Host changes (T17, T18, E5)

Done in bevy_factory `fce47b8` and `f9928a4`: a factory keeps running when its host leaves, under
the survivor the platform names. The checklist for another lockstep game:

| Where | Before | After | Covered by |
|---|---|---|---|
| `game/multiplayer.rs` `return_to_menu_when_client_lobby_is_lost` | the client lobby going away → menu | `LobbyLeft` → menu, and `LockstepMigrationFailed` → leave. A survivor named host keeps its lobby, which gains `Host` and is no longer a client lobby | `the_player_who_becomes_host_stays_in_the_world`, `everyone_returns_to_the_menu_when_no_host_is_named` |
| `game/player/mod.rs` `spawn_players_at_agreed_tick` | who to spawn from `LockstepLobbyParticipant` entities | who from `LockstepRoster`, which changes inside the tick; `joined_at_tick` only orders it. A host change removes the old host's participant at a different moment on each peer | `a_joiner_runs_every_tick_with_the_roster_the_host_ran_it_with` |
| `game/ui/help_screen.rs` | Leave | a host's Leave hands the game over; End Game for Everyone writes `CloseLobby` and does **not** write `LeaveLobby` (the core leaves once everyone is told) | `a_closed_lobby_ends_for_everyone_and_does_not_migrate` (ensemble) |
| `game/ui/layer_indicator.rs` | "(Paused)" while the clock is held | the reason: `AwaitingHost` (waiting for / reaching a new host), `LockstepMigration` (reporting, collecting, catching up), then who hosts | unit tests in the module |
| `src/testing/harness.rs` | — | `set_host_migration`, `lose_host`, `name_host`, `migrate`, `try_host`, `is_hosting`, `assert_peers_agree` (a crashed host's log is not the survivors') | `tests/host_migration.rs` |

**Watch:** a placement a player made just before the host left is resent to the new host and
built once (`a_placement_made_as_the_host_left_is_built_exactly_once_everywhere`). Anything the
old host alone had ruled past the resume tick is gone, as if it had not happened.
