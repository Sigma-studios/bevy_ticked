# bevy_kart

A 2D kart racer on avian2d, client/server over WebRTC. The game carried its own correction
smoothing, a wire-format epoch, placeholder position components and an avian setup; all of it
is upstream now.

| Step | Delete | Replaced by | Covered by |
|---|---|---|---|
| 2 | `src/wire_format.rs` (96 lines): `ProtocolEpoch`, "plugin add order is the wire format" | derived wire indices, `wire_hash()`, the registry handshake (T7, E2) | `registration_order_does_not_change_the_wire_format`, `a_mismatched_client_is_told_which_registration_differs` |
| 2 | `NetworkedPosition`, `NetworkedRotation` in `src/networking.rs` | avian's `Position`/`Rotation` on the wire under `avian::*` (T14) | `a_stack_of_boxes_replays_bit_identically_with_the_documented_bundle` |
| 2 | `OwnerPlayer(u128)` and its registration | `bevy_ticked_networking::Owner`, registered once-only by the stack (T8, T13) | `a_once_component_is_sent_exactly_once_per_entity` |
| 3 | `TickSource::FixedUpdate` + avian's `interpolate_all()` | `Hz(64.0)`, `TickedInterpolationPlugin` (T4) | `interpolation_never_feeds_the_blend_back_into_the_simulation` |
| 5 | the `Explosion` tracked entity spawned for a sound, the `GameTimer` client-side despawn | `TickedEvents` for one-shot effects; `despawn_ticked` (T10) | `a_despawn_rolled_back_resurrects_the_same_entity_id` |
| 5 | `RemovedComponents<Lobby>` cleanup | `LobbyLeft { reason }` (E1) | `a_peer_that_stops_answering_pings_is_despawned_after_the_timeout` |
| 6 | `src/rollback_smoothing.rs` (159 lines): `RollbackSmoothingPlugin`, `CorrectionSmoothing`, `ApplyCorrectionSet` | `TickedSmoothingPlugin`, `CorrectionSmoothing { decay_rate, max_offset, max_angle, apply_to }` (T8) | `a_small_correction_decays_and_never_snaps`, `correction_smoothing_never_touches_the_local_player` |
| 7 | `capture_local_input` in `Update` | `TickedInputPlugin::<PlayerInput>::new(sampler)` (T14) | `sampling_in_the_hook_costs_no_extra_frame` |
| 8 | the four `avian::*` registrations, `transform_to_position: false`, the solver tweaks | `bevy_ticked_avian::avian2d::TickedAvianPlugin` (T14) | `the_avian_stack_golden_matches` |

**Must change, not delete:** `rand::rng()` inside ticked systems. A replayed tick draws
different numbers; keep the RNG in a ticked resource seeded from the tick or a networked seed
(`docs/ROLLBACK_RULES.md` §3). The source guard from `bevy_ticked_testing::source_guard` flags
every reach; add `tests/sim_is_deterministic.rs` from the examples in this repository.

**Watch:** bodies the game positioned through `Transform` after spawn. The bundle turns avian's
`Transform` → `Position` sync off; move a kart by `Position`, or opt back in with
`positions_from_transforms()` and accept that a rollback then depends on nothing having touched
`Transform` between ticks.

## Host changes (T17, E5)

bevy_kart picks this up when it moves `bevy_ensemble` from `9f7f245` to the rev `bevy_ticked` pins
(`b71691a`), together with `bevy_ticked` `main`. Over WebRTC nothing migrates until the signalling
server runs E5b. After that, the lobby survives a host that leaves or crashes, and the bridge ends
the race and starts a fresh snapshot session under the new host. The race is not carried over; the
survivors go back to the lobby screen.

| Where | Today | On a host change | Covered by |
|---|---|---|---|
| `lobby.rs:328` `exit_lobby_when_session_ends` | both role resources gone → `OutOfLobby`, `OutOfGame` | **breaks first**: the bridge removes both roles for two frames on `HostChanged` and re-adopts them, so every survivor would drop to the start menu with its lobby still standing. Key it on the lobby entity being gone (or `LobbyLeft`), or skip it while `ReadoptAfterHostChange` exists | `a_host_change_ends_the_snapshot_session_for_every_survivor`, `the_new_host_and_the_client_that_stays_start_a_fresh_session` |
| `lobby.rs` (new system) | nothing reads `HostChanged` | on `HostChanged`: set `AppState::OutOfGame`, keep `LobbyState::InLobby`, reset `FinishTimes`/`RaceEnded` and `autostart_race`'s `Local` | `every_peer_is_told_who_the_host_became` (ensemble) |
| `lobby.rs:282` `enter_lobby` | `EnteredLobby` marks the one entry | nothing: the lobby entity is the same, so the marker stays and the screen must be rebuilt by hand (next row) | — |
| `menu/lobby.rs:275` `spawn_lobby`, `menu/map_picker.rs:53` `spawn_picker(is_host)` | Start button and map picker fixed from `is_host` on `OnEnter(Screen::Lobby)` | respawn the lobby screen on `HostChanged`. A peer already on it gets no `OnEnter`. Clear `ListedTracks` so the new host's `refresh_track_list` fills its list | — |
| `kart/mod.rs:336,423` | `is_host` cached per `LobbyCar`, kick buttons added at spawn | rebuild the cars' kick buttons on `HostChanged`; participants keep their entities, so no new `LobbyCar` is spawned | `participant_entities_and_player_data_survive_a_host_change` (ensemble) |
| `map_sync.rs:97` | host announces `MapSelected` when `SelectedMap` changes, and to each `Added<LobbyClient>` | the promoted peer keeps the last map it received; mark `SelectedMap` changed on `HostChanged` so it is announced. Followers are re-seated as `Added<LobbyClient>` on the new host and hear it from there | `the_new_host_and_the_client_that_stays_start_a_fresh_session` |
| `menu/start.rs:365` `report_join_failure` | exhaustive `match` on `LobbyLeftReason` | nothing: no variant was added. `HostGone` now arrives only after the wait for a new host (about 90 s over WebRTC) | `no_successor_before_the_deadline_ends_the_session_as_host_gone` (ensemble) |
| lobby screen | — | show `AwaitingHost { waited, successor }`: "the host left, waiting for a new one" / "reaching the new host" | `a_client_that_loses_its_host_keeps_its_lobby_and_waits` (ensemble) |
| `menu/lobby.rs:310` Leave | despawns the `Lobby` | a host's leave, by despawning or by `LeaveLobby`, now hands the lobby to the earliest-joined player. Give the host a "Close lobby" action that writes `CloseLobby` | `a_closed_lobby_ends_for_everyone_and_does_not_migrate` (ensemble) |
