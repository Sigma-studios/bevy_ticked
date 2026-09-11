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
