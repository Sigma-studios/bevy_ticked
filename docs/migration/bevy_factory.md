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
