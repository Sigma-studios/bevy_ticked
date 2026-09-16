# run-2d

A 2D arena shooter on avian2d with grenades and pellets, client/server. Its `docs/UPSTREAM.md`
(789 lines) is the record of what it needed from the stack; most of it landed, and its tests
came upstream as `crates/bevy_ticked_networking_ensemble/tests/lossy_links.rs`.

| Step | Delete | Replaced by | Covered by |
|---|---|---|---|
| 1 | `src/net_tests.rs` freeze-a-peer loop, the link presets | `TickedNetwork::freeze`, `Link::{cable, bad_wifi, satellite}` (E0, T3) | `lossy_links.rs` |
| 2 | `HistoryBufferTicks(256)` | sized by the networking plugins (T4) | `the_client_plugin_sizes_the_history_window` |
| 3 | `transform_to_position: false` | `TickedAvianPlugin` (T14) | avian suite |
| 4 | `reset_round_on_leave` | `TickedResource: Default` and `reset_all` on leave (T4) | `reset_on_leave_resets_registered_resources` |
| 5 | host-only pellet and grenade spawns | `spawn_by(local_slot, ..)` on every peer; the authority confirms under the same id (T10) | `a_grenade_does_not_blink_on_the_client`, `a_predicted_bullet_survives_the_snapshot_that_predates_it` |
| 6 | `src/smoothing.rs` (277 lines): `DrawOffset`, `Smoothed`, `CorrectionStats`, `measure_corrections`, `draw_smoothed` | `TickedSmoothingPlugin`, `CorrectionStats`, `measure_prediction::<T>` (T8) | `remote_bodies_no_longer_snap_at_every_snapshot` |
| 6 | the `NetInput` hold-last half | `InputQueue::at_tick_or_last`, relayed inputs (T8) | `a_predicted_remote_body_holds_its_last_input_during_replay` |
| 7 | `local_input_plugin`, `LocalPlayerId`/`SoloPlayer` | `TickedInputPlugin`, `LocalPlayer` (T14) | `input_sampled_by_the_plugin_is_stamped_for_the_tick_it_will_run_in` |
| 9 | `warn_when_this_peer_has_no_body` | `HealthWarnings` (T3) | `diagnostics.rs` |

**Watch:** the grenade's fuse and blast were written host-only because a client could not
despawn predictively. They can now (`despawn_ticked`, revived if the host disagrees), so the
client's grenade and the host's are the same entity id from the throw onward.

## Host changes (T17, E5)

run-2d picks this up when it moves `bevy_ensemble` from `9f7f245` to the rev `bevy_ticked` pins
(`b71691a`), together with `bevy_ticked` `main`. Over WebRTC nothing migrates until the signalling
server runs E5b. After that, the lobby survives a host that leaves or crashes. The bridge
(`TickedEnsembleSessionPlugin`) ends the snapshot session on `HostChanged`, which resets every
registered ticked resource (`RoundState` included) and the tracked world, then re-adopts roles under
the new host. `sync_screen` then shows `Screen::Lobby` from the reset `RoundState`, so most of the
way back to the lobby already happens without game code.

| Where | Today | On a host change | Covered by |
|---|---|---|---|
| `session.rs:350` `reset_round_on_leave` | runs when the lobby goes; resets `BuiltArena`, `StartMatchRequest`, `PracticeArena`, `JoinedCode`, `CurrentTick` | also run on `HostChanged`, minus `JoinedCode`, which still names the lobby. `RoundState` is reset by the bridge; the plain resources are not, and a `StartMatchRequest` or `BuiltArena` left over would open the new host's lobby straight into the old match | `a_host_change_ends_the_snapshot_session_for_every_survivor` |
| `session.rs:89` `is_authority()` | "no `LocalClientPlayer`" | for two frames after `HostChanged` a follower holds neither role and counts as the authority. Test for `LocalServerPlayer`, or for no lobby at all, instead | `the_new_host_and_the_client_that_stays_start_a_fresh_session` |
| `player.rs:215` `spawn_players` | fills the roster from a `(Lobby, Host)` | nothing: a promotion inserts `Host` on the same lobby, and participants keep their entities | `the_new_host_hands_out_spawner_slots_from_one` |
| `menu.rs:1379` `show_start_button`, `:1401` `show_lobby_code` | read the lobby every frame | nothing: START follows `Host`, and the WebRTC backend puts the lobby's code on the promoted lobby | — |
| lobby screen | — | show `AwaitingHost { waited, successor }` while the match is frozen: "the host left, waiting for a new one" / "reaching the new host". Over WebRTC the wait lasts up to about 90 s | `a_client_that_loses_its_host_keeps_its_lobby_and_waits` (ensemble) |
| `menu.rs:700` LEAVE, `session.rs:402` `leave` | despawns the lobbies | a host's leave now hands the lobby to the earliest-joined player. Add an action that writes `CloseLobby` if the host should end it for everyone | `a_closed_lobby_ends_for_everyone_and_does_not_migrate` (ensemble) |

**Keep:** `sync_screen`'s "no lobby → `Screen::Menu`". It still ends the session after an unanswered
wait (`LobbyLeft { HostGone }`) and after `CloseLobby`.
