# bevy_ticked Implementation Plan

## Architecture Overview

A **tick-driven state management system** for Bevy with rollback networking support. The tick system is **fully independent from Bevy's schedules** — it owns a dedicated `TickedSimulation` schedule. FixedUpdate is just one possible driver that triggers tick advancement when unpaused.

### Workspace Structure

```
bevy_ticked/
  Cargo.toml                     # workspace root + core crate (bevy_ticked)
  src/
    lib.rs                       # TickedPlugin, re-exports
    prelude.rs                   # public API surface
    tick.rs                      # CurrentTick, TickConfig, tick advancement
    registry.rs                  # TickedComponentRegistry, registration, type-erased dispatch
    world_actions.rs             # WorldActions<T> — per-tick per-entity component history
    entity_network_id.rs         # EntityNetworkId component + counter
    snapshot.rs                  # WorldSnapshot serialization format
    rollback.rs                  # rollback_to_tick, rollback_and_resimulate
  crates/
    bevy_ticked_multiplayer/
      Cargo.toml
      src/
        lib.rs                   # TickedMultiplayerPlugin, re-exports
        prelude.rs
        messages.rs              # NetworkSnapshot, NetworkInput — global observer events
        input.rs                 # InputQueue<T>, TickedInput trait
        server.rs                # TickedServerPlugin — tick loop, snapshot broadcast
        client.rs                # TickedClientPlugin — prediction, rollback-on-snapshot
    bevy_ticked_multiplayer_ensemble/
      Cargo.toml
      src/
        lib.rs                   # Bridge: ensemble messages <-> multiplayer observers
  examples/
    bouncing_ball.rs             # avian3d demo with tick controls
```

---

## Phase 1: Core Tick System (`bevy_ticked`)

### 1.1 Convert to Workspace

Transform root `Cargo.toml` into workspace + library crate. Move `main.rs` content to example.

```toml
[package]
name = "bevy_ticked"
version = "0.1.0"
edition = "2024"

[workspace]
members = ["crates/bevy_ticked_multiplayer", "crates/bevy_ticked_multiplayer_ensemble"]

[dependencies]
bevy = "0.18"
postcard = { version = "1", features = ["alloc"] }
serde = { version = "1", features = ["derive"] }

[dev-dependencies]
avian3d = "0.5"
```

### 1.2 Core Types (`tick.rs`)

```rust
#[derive(Resource, Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CurrentTick(pub u64);

#[derive(Resource, Clone, Debug)]
pub struct TickConfig {
    pub paused: bool,
}

// Events for manual control
#[derive(Event)]
pub struct StepForward;

#[derive(Event)]
pub struct StepBackward;

#[derive(Event)]
pub struct ResetToTick(pub u64);
```

### 1.3 TickedSimulation Schedule

A standalone schedule that the tick system owns. **All game simulation systems go here** — not in FixedUpdate.

```rust
#[derive(ScheduleLabel, Clone, Debug, PartialEq, Eq, Hash)]
pub struct TickedSimulation;
```

**Execution flow:**
- **Normal play (unpaused):** A system in `FixedUpdate` calls `world.run_schedule(TickedSimulation)` and increments `CurrentTick`.
- **Rollback:** Loop calling `world.run_schedule(TickedSimulation)` for each tick being replayed.
- **Manual step forward:** Run `TickedSimulation` once, increment tick.
- **Manual step backward:** Restore state from history at `CurrentTick - 1`, decrement tick. No re-simulation needed.
- **Reset to tick N:** Restore state from history at tick N, set `CurrentTick = N`.

State capture (saving to WorldActions) happens **after** each TickedSimulation run, as part of the tick advancement logic — not inside the schedule itself.

### 1.4 EntityNetworkId (`entity_network_id.rs`)

```rust
#[derive(Component, Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct EntityNetworkId(pub u64);

#[derive(Resource, Default)]
pub struct EntityNetworkIdCounter(pub u64);

impl EntityNetworkIdCounter {
    pub fn next(&mut self) -> EntityNetworkId {
        self.0 += 1;
        EntityNetworkId(self.0)
    }
}
```

### 1.5 Component Registry (`registry.rs`)

Mirrors bevy_ensemble's registry pattern — sequential `u16` indices, type-erased function pointer dispatch.

```rust
pub trait TickedComponent: Component + Serialize + DeserializeOwned + Clone + Send + Sync + 'static {}
// Auto-impl for all qualifying types

#[derive(Resource, Default)]
pub struct TickedComponentRegistry {
    entries: Vec<RegisteredTickedComponent>,
    type_indices: HashMap<TypeId, u16>,
}

struct RegisteredTickedComponent {
    type_name: &'static str,
    capture: fn(&mut World, u64),                              // capture all entities' T at tick
    restore: fn(&mut World, u64),                              // restore all entities' T from tick
    serialize_at: fn(&World, u64) -> Option<HashMap<u64, Vec<u8>>>,  // for snapshots
    deserialize_and_apply: fn(&mut World, u64, &HashMap<u64, Vec<u8>>), // from snapshots
}
```

Registration extension trait:
```rust
pub trait TickedAppExt {
    fn register_ticked_component<T: TickedComponent>(&mut self) -> &mut Self;
}
```

`register_ticked_component::<T>()`:
1. Assigns sequential `u16` index (order must match on all peers)
2. Stores monomorphized function pointers for capture/restore/serialize/deserialize
3. Initializes `WorldActions<T>` resource

### 1.6 WorldActions (`world_actions.rs`)

One resource per registered component type:

```rust
#[derive(Resource)]
pub struct WorldActions<T: TickedComponent> {
    history: HashMap<u64, HashMap<u64, T>>,  // tick -> (entity_network_id -> component)
}
```

**Capture:** Query all `(&EntityNetworkId, &T)`, clone into `history[current_tick]`.

**Restore:** Read `history[target_tick]`, overwrite components on matching entities.
- Entities in history but missing from world: skip (entity lifecycle is separate concern)
- Entities in world but missing from history at that tick: remove component

### 1.7 Snapshot (`snapshot.rs`)

Serializable world state at a tick:

```rust
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorldSnapshot {
    pub tick: u64,
    pub components: HashMap<u16, HashMap<u64, Vec<u8>>>,  // component_type_index -> entity_id -> bytes
}
```

- **Build:** For each registered component, serialize all entities' state at a given tick via registry dispatch
- **Apply:** For each component type in snapshot, deserialize and overwrite via registry dispatch

### 1.8 Rollback (`rollback.rs`)

```rust
/// Restore world state to a previous tick from WorldActions history.
pub fn rollback_to_tick(world: &mut World, target_tick: u64);

/// Rollback to target_tick, then re-simulate forward to end_tick.
/// Runs TickedSimulation schedule once per tick, capturing state after each.
pub fn rollback_and_resimulate(world: &mut World, target_tick: u64, end_tick: u64);
```

`rollback_and_resimulate` loop:
1. `rollback_to_tick(world, target_tick)`
2. For tick in `(target_tick + 1)..=end_tick`:
   - Set `CurrentTick` to tick
   - `world.run_schedule(TickedSimulation)`
   - Capture state for this tick into WorldActions

### 1.9 Plugin (`lib.rs`)

```rust
pub struct TickedPlugin;

impl Plugin for TickedPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<CurrentTick>()
           .init_resource::<TickConfig>()
           .init_resource::<TickedComponentRegistry>()
           .init_resource::<EntityNetworkIdCounter>()
           .init_schedule(TickedSimulation)
           .add_event::<StepForward>()
           .add_event::<StepBackward>()
           .add_event::<ResetToTick>()
           .add_systems(FixedUpdate, advance_tick_system)  // drives ticks when unpaused
           .add_systems(PreUpdate, handle_manual_controls); // step/reset from keyboard
    }
}
```

`advance_tick_system`: If not paused, increment tick, run `TickedSimulation`, capture state.

`handle_manual_controls`: Read step/reset events, perform rollback or single-step accordingly.

---

## Phase 2: Bouncing Ball Example

### 2.1 Setup

Uses `bevy_ticked` core only (no multiplayer). Demonstrates tick controls with avian3d physics.

**Avian integration:** Use `PhysicsPlugins::new(TickedSimulation)` so avian's physics systems run inside our tick schedule. This is the "manual stepping" approach — physics only runs when we explicitly step a tick.

### 2.2 Example Code Outline

```rust
fn main() {
    App::new()
        .add_plugins((
            DefaultPlugins,
            TickedPlugin,
            PhysicsPlugins::new(TickedSimulation).with_length_unit(1.0),
        ))
        .register_ticked_component::<Transform>()
        .register_ticked_component::<LinearVelocity>()
        .register_ticked_component::<AngularVelocity>()
        .register_ticked_component::<Position>()
        .register_ticked_component::<Rotation>()
        .add_systems(Startup, setup_scene)
        .add_systems(Update, (keyboard_controls, update_tick_ui))
        .run();
}
```

**Scene:** Camera, directional light, ground plane (static rigidbody + collider), bouncing ball (dynamic rigidbody + sphere collider + EntityNetworkId).

**Controls:**
- Space: toggle play/pause
- Right arrow: step forward one tick (while paused)
- Left arrow: step backward one tick (while paused)
- R: reset to tick 0

**UI:** Text overlay showing `Tick: {N}` and `[PAUSED]`/`[PLAYING]` status.

---

## Phase 3: Multiplayer Abstraction (`bevy_ticked_multiplayer`)

### 3.1 Input Trait and Queue (`input.rs`)

```rust
pub trait TickedInput: Serialize + DeserializeOwned + Clone + Send + Sync + 'static {}

#[derive(Resource)]
pub struct InputQueue<T: TickedInput> {
    pub inputs: HashMap<u64, HashMap<u128, T>>,  // tick -> player_uuid -> input
}
```

### 3.2 Network Events — Global Observers (`messages.rs`)

Abstract events that any transport can trigger/observe:

```rust
// Incoming — transport layer triggers these via commands.trigger()
#[derive(Event)]
pub struct ReceivedNetworkSnapshot(pub WorldSnapshot);

#[derive(Event)]
pub struct ReceivedNetworkInput<T: TickedInput> {
    pub sender: u128,  // PlayerUUID
    pub tick: u64,
    pub input: T,
}

// Outgoing — multiplayer systems trigger these, transport layer observes
#[derive(Event)]
pub struct SendNetworkSnapshot(pub WorldSnapshot);

#[derive(Event)]
pub struct SendNetworkInput<T: TickedInput> {
    pub tick: u64,
    pub input: T,
}
```

### 3.3 Server Plugin (`server.rs`)

`TickedServerPlugin<T: TickedInput>`

Systems:
1. **FixedUpdate — tick loop:** Collect inputs from `ReceivedNetworkInput<T>` into `InputQueue<T>`, advance tick, run `TickedSimulation`, capture state, trigger `SendNetworkSnapshot` with the tick's world snapshot.
2. **Input application:** A system inside `TickedSimulation` reads `InputQueue<T>` for the current tick and applies inputs (user provides this system via a callback/trait).
3. **Self-hosting:** If server is also a client, own inputs are written directly to `InputQueue<T>` for the current tick.

### 3.4 Client Plugin (`client.rs`)

`TickedClientPlugin<T: TickedInput>`

Systems:
1. **FixedUpdate — tick loop:** Apply own input locally (prediction), store in `InputQueue<T>`, trigger `SendNetworkInput<T>`, advance tick, run `TickedSimulation`, capture state.
2. **Snapshot handling (PreUpdate):** When `ReceivedNetworkSnapshot` arrives:
   - Save current tick
   - `rollback_to_tick(snapshot.tick)`, apply snapshot
   - For each tick from `snapshot.tick + 1` to saved current tick: apply local inputs from `InputQueue`, run `TickedSimulation`, capture
   - This is `rollback_and_replay_local_inputs_to_now(snapshot)`

### 3.5 Registration

```rust
pub trait TickedMultiplayerAppExt {
    fn register_ticked_input<T: TickedInput>(&mut self) -> &mut Self;
}
```

Registers the input type, initializes `InputQueue<T>`, adds observer listeners for the network events.

---

## Phase 4: Ensemble Bridge (`bevy_ticked_multiplayer_ensemble`)

### 4.1 Plugin

Thin glue layer:

```rust
pub struct TickedMultiplayerEnsemblePlugin<T: TickedInput> { ... }
```

### 4.2 Message Types

Serializable wrappers registered as ensemble message types:

```rust
#[derive(Message, Clone, Serialize, Deserialize)]
pub struct NetworkSnapshotMessage(pub WorldSnapshot);

#[derive(Message, Clone, Serialize, Deserialize)]
pub struct NetworkInputMessage<T: TickedInput> {
    pub tick: u64,
    pub input: T,
}
```

### 4.3 Bridge Systems

**Ensemble -> Multiplayer (incoming):**
- Read `ReceivedEnsembleMessage<NetworkSnapshotMessage>` via `MessageReader`, trigger `ReceivedNetworkSnapshot` global observer
- Read `ReceivedEnsembleMessage<NetworkInputMessage<T>>`, trigger `ReceivedNetworkInput<T>` global observer

**Multiplayer -> Ensemble (outgoing):**
- Observe `SendNetworkSnapshot`, trigger `LobbyMessage<NetworkSnapshotMessage>` on lobby entity
- Observe `SendNetworkInput<T>`, trigger `LobbyMessage<NetworkInputMessage<T>>` on lobby entity

### 4.4 Registration

```rust
app.register_ensemble_message_type::<NetworkSnapshotMessage>()
   .register_ensemble_message_type::<NetworkInputMessage<T>>();
```

---

## Implementation Order

1. **Workspace setup** — Convert Cargo.toml, create dirs, create stub crate files
2. **Core types** — `CurrentTick`, `TickConfig`, `EntityNetworkId`, events
3. **TickedSimulation schedule** — Init schedule, `advance_tick_system`, pause/step logic
4. **Registry** — `TickedComponentRegistry`, `register_ticked_component`, function pointer dispatch
5. **WorldActions** — `WorldActions<T>`, capture system, restore system
6. **Rollback** — `rollback_to_tick`, `rollback_and_resimulate`
7. **Snapshot** — `WorldSnapshot` serialize/deserialize via registry dispatch
8. **Plugin assembly** — Wire everything in `TickedPlugin`
9. **Bouncing ball example** — avian3d scene, keyboard controls, UI
10. **Multiplayer crate** — Events, InputQueue, server plugin, client plugin
11. **Ensemble bridge crate** — Message wrappers, forwarding systems

---

## Key Design Decisions

| Decision | Choice | Rationale |
|----------|--------|-----------|
| Serialization | serde + postcard | Consistent with bevy_ensemble, compact binary format |
| Schedule | Independent `TickedSimulation` | Fully decoupled from Bevy scheduling, clean rollback |
| Registry pattern | Sequential u16 indices | Mirrors bevy_ensemble, deterministic across peers |
| Avian integration | `PhysicsPlugins::new(TickedSimulation)` | Physics runs inside tick schedule, automatic during rollback |
| Transport abstraction | Global observers | Immediate processing, matches user requirement |
| Component trait bounds | `Component + Serialize + DeserializeOwned + Clone` | Minimum needed for capture/restore/snapshot |
