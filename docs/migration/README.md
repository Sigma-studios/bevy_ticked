# Migrating a game

One checklist per game that consumes `bevy_ticked`, each in the order the changes have to be
applied. `../MIGRATION.md` explains every change; these say which of them a given game
meets and what it gets to delete.

Apply order, the same for every game:

1. **Pin** `bevy_ensemble` at the commit `bevy_ticked` pins (`Cargo.toml`, `[workspace.dependencies]`) and
   take `bevy_ticked_testing` as a dev-dependency.
2. **Registrations**: every networked type named (`register_networked_ticked_component::<T>("Name")`),
   resources likewise; delete index assertions and epoch constants; kind markers and owners `_once`.
3. **Tick source**: `TickSource::Hz(64.0)`; the networking plugins refuse `FixedUpdate`.
4. **Pause**: `TicksPaused` → `TickHolds` with your own `TickHoldReason`; session pauses through
   `PauseSession`/`ResumeSession` and `PausePolicy`.
5. **Spawn and despawn**: `TrackedSpawner`/`spawn_by(slot, ..)`, `despawn_ticked`; delete the
   authority module and the counter.
6. **Replication modes**: nothing to do for remote bodies (`Interpolated` is the default and the
   bridge marks the local body `Predicted`); delete the smoothing module; opt kinematic bodies in.
7. **Input**: `TickedInputPlugin::<I>::new(sampler)`; delete the `Update` capture system.
8. **Physics**: `TickedAvianPlugin`; delete the `avian::*` registrations and the solver tweaks.
9. **Diagnostics and tests**: replace the game's counters with `bevy_ticked_networking::diagnostics`
   and its harness with `bevy_ticked_testing`.

Each game's file lists what it deletes at each step and which upstream test now covers the
behaviour the deleted code was protecting.
