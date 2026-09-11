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
