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
