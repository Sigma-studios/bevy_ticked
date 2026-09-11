# Plan

The implementation plan that used to live here described crates that no longer exist
(`bevy_ticked_multiplayer`, `bevy_ticked_multiplayer_ensemble`) and a snapshot format that was
replaced. It was kept long past its usefulness and read as current by more than one person.

What is current:

- **The crates and how they fit together:** `ARCHITECTURE.md`.
- **What changed and how to migrate a game:** `docs/MIGRATION.md`, appended one section per
  phase of the overhaul, and the per-game checklists under `docs/migration/`.
- **The rules a simulation has to follow to roll back:** `docs/ROLLBACK_RULES.md`.

The overhaul itself is tracked outside this repository, phase by phase, one branch and one pull
request per phase, merged in order.
