# una_zombies

A first-person zombie shooter on avian3d, client/server, with a scripted two-peer soak
(`scripts/two-peer.sh`) whose rules became `scripts/netpeers.sh` here. Its
`docs/upstream-needs.md` (892 lines) tracked what it patched around.

| Step | Delete | Replaced by | Covered by |
|---|---|---|---|
| 2 | the `WIRE_FORMAT` golden tests and the `TickTrackedEntity` registration | derived indices, `wire_hash()`; `TickTrackedEntity` is never a wire type (T7) | `wire_v2.rs` |
| 2 | `net/identity.rs::OwnerPlayer` | `bevy_ticked_networking::Owner` (T8) | `remote_modes.rs` |
| 3 | `TickSource::FixedUpdate` + `interpolate_all()`, `assert_tick_delta` | `Hz(64.0)`; inside the tick `Time::delta()` is the tick (T4) | `the_frame_clocks_read_tick_values_inside_the_simulation` |
| 4 | the `InputQueue` sort before folding over players | `at_tick` returns a `BTreeMap` (T4) | `input_queue_at_tick_iterates_in_a_fixed_order` |
| 5 | `net/authority.rs` (78 lines): `spawn_tracked`, `LocalPlayerUuid` | `TrackedSpawner`, `LocalSpawnerSlot`, `LocalPlayer` (T10, T14) | `two_clients_spawning_in_the_same_tick_never_collide_on_ids` |
| 5 | `RocketInFlight` derived on the shooter only | spawn the rocket with `spawn_by(local_slot, ..)` on the shooter; it is replicated like any body (T10) | `a_spawn_survives_a_dropped_snapshot` |
| 6 | `PlayerInputState` hold-last | `at_tick_or_last` (T8) | `get_or_last_holds_the_last_known_input` |
| 7 | `net/input.rs` capture in `Update` | `TickedInputPlugin` (T14) | `a_keypress_reaches_the_server_with_the_configured_margin` |
| 8 | the `avian::*` registrations | `TickedAvianPlugin` (T14) | avian suite |
| 9 | `report_snapshot_size`, `SOLO_PLAYER_UUID = LOCAL_PLAYER_UUID` | `SnapshotStats`; a constant of the game's own (T3, E1) | `diagnostics.rs` |

**Keep:** the rule the soak script wrote down — assert on the peer under test, and find the
guest's uuid in the host's log. `scripts/netpeers.sh` enforces it for the examples; the game's
own checks (`client-kill`, `revive`, `rounds`) stay the game's.

## Host changes (T17, E5)

una_zombies picks this up on its next `cargo update` of `bevy_ticked` (`main`, locked at `824dd20`)
and `bevy_ensemble` (`master`, locked at `a389648`); move them together. Over WebRTC nothing migrates
until the signalling server runs E5b. After that, the lobby survives a host that leaves or crashes,
and the bridge ends the snapshot session and starts a fresh one under the new host. The world is not
carried over: the survivors go back to the pre-round lobby (`RoundPhase::Waiting`) under the new host.

| Where | Today | On a host change | Covered by |
|---|---|---|---|
| `app/state.rs` (new system) | nothing reads `HostChanged`, and nothing leaves `InGame` except the Leave button | on `HostChanged`: go to `AppState::Menu` and remove `LaunchRequest`, leaving the lobby alone. `OnExit(InGame)` runs `tear_the_world_down` (`app/teardown.rs:57`), which already clears `RoundState`, `RoundTimers`, `AmmoRespawn`, `StartRoundRequested`, `RestartRequested` and `Paused`. Once the bridge re-adopts a role, `open_a_world` (`net/lobby.rs:135`) asks for a world again and `init_world_state` (`sim/round.rs:383`) sets it up in `Waiting` | `a_host_change_ends_the_snapshot_session_for_every_survivor`, `the_new_host_and_the_client_that_stays_start_a_fresh_session` |
| `sim/round.rs:383` `init_world_state` | skips when `RoundState` exists | nothing, provided the world is torn down as above: a `RoundState` left over from the old host would stop the new one from setting up its round | — |
| `sim/spawn.rs:45,125` `spawn_for_participants` | needs `LocalServerPlayer` and `(Lobby, Host)` | nothing: a promotion inserts `Host` on the same lobby, and participants keep their entities, so the new host spawns a body for every survivor | `the_new_host_hands_out_spawner_slots_from_one`, `participant_entities_and_player_data_survive_a_host_change` (ensemble) |
| `net/profile.rs:197` `publish_profile` | remembers the lobby entity it told | nothing: `PlayerData<PlayerProfile>` stays on the participants through the change, and the lobby entity is the same | `participant_entities_and_player_data_survive_a_host_change` (ensemble) |
| `net/lobby.rs:113` `announce_session` | latch reset when both roles are gone | nothing: the bridge removes both roles on `HostChanged`, so the new host logs "hosting as" again | — |
| in-game HUD | — | show `AwaitingHost { waited, successor }` while the world is frozen: "the host left, waiting for a new one" / "reaching the new host". Over WebRTC the wait lasts up to about 90 s | `a_client_that_loses_its_host_keeps_its_lobby_and_waits` (ensemble) |
| session end | nothing moves `InGame` back to `Menu` when the lobby goes (**already true today**) | read `LobbyLeft` and go to `AppState::Menu`: after an unanswered wait (`HostGone`), after `CloseLobby`, after a kick | `no_successor_before_the_deadline_ends_the_session_as_host_gone` (ensemble) |
| `ui/menu.rs:1294` Leave | despawns the lobbies | a host's leave now hands the lobby to the earliest-joined player. If the host should be able to end the game for everyone, add an action that writes `CloseLobby` | `a_closed_lobby_ends_for_everyone_and_does_not_migrate` (ensemble) |

**Keep:** the `is_authoritative` run conditions and `ui/gameover.rs:141`. They are read every frame,
so they follow the role to the new host.
