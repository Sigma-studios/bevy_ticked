# Migration

One section per phase of the netcode overhaul, newest first. Each names what broke, what to
change in a game, and why. Both peers of a session must be built from the same commit.

Phases that changed `bevy_ensemble` too say which of its commits they pin; that crate's own
`docs/MIGRATION.md` covers what changed there.

**Migrating a game:** `docs/migration/README.md` has the order to apply the changes in, and
`docs/migration/<game>.md` what each of the six games deletes at each step. `ARCHITECTURE.md`
is the map of the crates as they stand; `docs/ROLLBACK_RULES.md` the rules a simulation
obeys; `docs/avian.md` the physics bundle.

## Renames, at a glance

| Before | After | Phase |
|---|---|---|
| `register_networked_ticked_component::<T>()` (unnamed) | `register_networked_ticked_component::<T>("Name")`, `_once`, `_as(name, class)` | T7, T13 |
| `TicksPaused` | `TickHolds` + `TickHoldReason` | T5 |
| `TickTrackedEntityCounter` | `TrackedIdAllocator`, `TrackedSpawner::{spawn, spawn_by}`, `LocalSpawnerSlot` | T10 |
| `EntityCommands::despawn` on a tracked entity | `despawn_ticked` | T10 |
| `WorldSnapshot`, `SnapshotRecipients(usize)` | `SnapshotPacket { seq, tick, your_margin, body }`, `SnapshotRecipientList(Vec<u128>)` | T7 |
| `SendNetworkSnapshot(bytes)` | `SendNetworkSnapshot { recipient, bytes }` | T7 |
| `NetworkInputPayload { inputs }` | `NetworkInputPayload { inputs, ack, nack_full }` | T7, T13 |
| `TickedServerPlugin::new()` alone | `.send_every(n)`, `.keyframe_every(n)`, `.compression(..)`, `.send_rates(..)` | T9, T13 |
| a game's `OwnerPlayer`/`PlayerUuid` | `bevy_ticked_networking::Owner` | T8 |
| a game's `capture_local_input` in `Update` | `TickedInputPlugin::<I>::new(sampler)`, `LocalPlayer` | T14 |
| a game's `avian::*` registrations and solver tweaks | `bevy_ticked_avian::{avian3d, avian2d}::TickedAvianPlugin` | T14 |
| `bevy_ticked_lockstep_networking::checksum` | `bevy_ticked::checksum` (re-exported) | T3 |
| `LOCAL_PLAYER_UUID` | gone; `Option<Res<LocalMultiplayerPlayerId>>` | T3, E1 |
| `Single<Lobby>` in a game's session code | `TickedSessionLobby(Entity)`, `TickedEnsembleSessionPlugin` | T7 |
| `AuthoritativeTick { tick, players_actions }` | `+ system: Vec<SystemAction>, margins` | T12 |
| `LockstepConfig::host_tick_buffer` default 4 | 1 | T12 |
| `TickedPlugin::default()` for a networked game | `TickedPlugin { source: TickSource::Hz(64.0), .. }` | T4 |

## T16 — warm starting is kept

No API or wire change. `TickedAvianPlugin` no longer zeroes `SolverConfig::warm_start_coefficient`.

**Before** the bundle zeroed warm starting on the reasoning that a replayed tick has a
different previous step to seed from. But the bundle also rolls the contact graph back, and
the previous step's impulses are in it: the stack of boxes replays bit-identically with warm
starting on (`crates/bevy_ticked_avian/tests/determinism.rs`, now run that way). What zeroing
it did cost was found in bevy_kart: a kart driven into a wall under a constant force was held
there, and two seconds of reverse moved it nothing at all, where the same build with warm
starting pulled away at once. Stacked contacts never accumulated the impulse to separate.

**After** warm starting is left as the game set it (avian's default). `keep_warm_starting()`
still compiles and is the default; `zero_warm_starting()` is the new opt-in. **Delete** a
`.keep_warm_starting()` a game added to work around it. **Watch** a golden trace recorded
under the bundle: the solver's numbers change, re-record with `UPDATE_GOLDEN=1`.

## T15 — documentation, `netpeer`, multi-process tests, CI

No API change in this repository. `bevy_ensemble` is pinned at `9f7f245` (E4): the first
real join over WebRTC found two ordering bugs in E2's join handshake, both fixed there.

- `examples/netpeer.rs` (`bevy_ticked_networking_ensemble`): one peer of a real session in
  its own process, headless, driven by arguments; writes a checksum line per confirmed tick.
- `tests/webrtc_multiprocess.rs`: a signalling server on a throwaway port and two or three
  `netpeer` processes; ignored by default, run with `--ignored --test-threads=1` (the
  `multiprocess` CI job does). Two and three processes agree on 400+ shared ticks; a client
  sees the host's world 2.5 s after starting; nobody logs a warning after the session starts.
- `scripts/netpeers.sh {session,join,soak}` against any signalling server, with una_zombies'
  rules: assert on the peer under test, cut logs at `LOG_SESSION_START`, exit 3 when the
  transport never connected.
- `ARCHITECTURE.md`, `CHANGELOG.md`, `docs/migration/`, `.github/workflows/ci.yml`.
- `bevy_ticked_networking_ensemble::local_session` (feature `local-session`): one shell, N
  windows. `TICKED_LOCAL_SESSION=2 cargo run --example fps_shooter` starts a signalling
  server in-process, launches a second copy of the executable as a client, and hosts; the
  copies host and join on their own. A game adds two lines: `server_url:
  local_session::signalling_url()` on its WebRTC plugin and `TickedLocalSessionPlugin`.
  `TICKED_ROLE=host|client` alone drives one process against `SIGNALLING_SERVER_URL`.

**Do** keep the client role adoption on a *promoted* lobby (the bridge does now:
`adopt_role` ignores `PendingLobby` for clients). **Watch** a game's own per-entity sends:
`EntityCommands::trigger` on a client that left in the same frame panics under the default
error handler; the bridge's sends go through `trigger_if_alive`.

## T14 — the avian bundle and the input plugin

No wire change. Rebuild every peer anyway: `TickedSystems` gained a set.

### `bevy_ticked_avian`

**Before** every game registered avian's four body components by hand, ran
`PhysicsPlugins::new(TickedSimulation)`, and either turned warm starting and sleeping off or
did not know it had to. **After** `TickedAvianPlugin` (one per dimension:
`bevy_ticked_avian::avian3d::TickedAvianPlugin`, `::avian2d::TickedAvianPlugin`) does the
registrations under `avian::*`, keeps warm starting (T16; it zeroed it at first), disables
sleeping, turns avian's
`Transform` → `Position` sync off (placing a body spawned with a `Transform` once), and
rolls back the solver's own state — `ContactGraph`, `ConstraintGraph`, `PhysicsIslands`,
`JointGraph`, `BodyIslandNode` — which a replay read before it read any body and which
nothing restored. `docs/avian.md` has the table and the measurements.

**Do** add it after `TickedPlugin` (and after your own `PhysicsPlugins` if you add them);
put your systems in `TickedSimulationSet::{Input, BeforePhysics, AfterPhysics}`; place
bodies by `Position`. **Delete** the `avian::*` registrations, `transform_to_position:
false`, and any `SolverConfig`/sleeping configuration the plugin now owns. **Watch** a
game that positioned bodies through `Transform` after spawn: that path is off
(`positions_from_transforms()` to keep it, solo only).

### `TickedInputPlugin`

**Before** every game had a `capture_local_input` system in `Update` that read the
keyboard, chose between `LocalClientPlayer` and `LocalServerPlayer`, and wrote
`queue.insert(tick + 1, uuid, input)`: once per frame (so a two-tick frame fed the second
tick its predecessor's input) and after the tick (so a keypress waited a frame). **After**
`TickedInputPlugin::<I>::new(sampler)` runs the sampler inside `TickedLoop` in the new
`TickedSystems::SampleInput` set — after the rollback, before the tick, once per tick — and
files what it returns for the tick about to run under `LocalPlayer`, which the role plugins
keep current (the host's uuid, the client's, `0` solo). The sampler is an ordinary system
returning `I` or `Option<I>`. It is skipped on a restore pass and while the clock is held.

**Do** turn the capture system into a sampler (drop the queue, the tick and the two role
resources; return the input) and add the plugin. **Delete** the `Update` registration.
Measured: the press is read by a tick in the same frame (the `Update` capture: the next).

### Registry

`register_ticked_resource_kept_on_leave::<R>()`: rolled back like any ticked resource, but
not reset to `Default` when the session ends, for a resource another library keeps
consistent with the world on its own.

## T13 — delta replication, replicate-once, send rates, compression

The wire is unchanged in shape (`PROTOCOL_VERSION` stays 2: the `Delta` body was reserved in
T7) but every packet now starts with a one-byte compression tag, and the input packet gained
`nack_full`. Rebuild every peer.

### What the host sends

**Before** every snapshot was the whole world, to every client, every tick. **After** the
host keeps the last few packets it sent each client (`max_unacked_baselines`, 32) and, once
the client has acknowledged one, sends a `Delta` against it: the records whose bytes
changed, component by component, the components that went (`removed`), the ids that died
(`despawned`), the resources that changed, and the relayed inputs. A full body still goes
to a joiner, after a nack, on every `keyframe_every`th packet (64), and whenever the
acknowledged baseline has fallen off the ring. `TickedServerPlugin::new()` takes
`keyframe_every(n)`, `compression(Compression)`, `send_rates(SendRates)` and
`without_deltas()`.

A client rebuilds the whole body from the baseline the moment a delta arrives
(`AuthoritativeHistory::body_at_seq`); the fast path, the rollback and the drawn history
never see a delta. A delta against a baseline the client no longer holds is dropped,
counted (`ReplayStats::dropped_unknown_baseline`), and answered with `nack_full: true` on
the next input packet, which makes the host's next packet a keyframe.

The acknowledgement rides the input packet, and a client at rest with nothing to send
still sends one when its acknowledgement changes: without it the host would fall back to
keyframes the moment a player stopped moving. Measured with the harness: 21.6 bytes a tick
from host to a client at rest, 47.5 with two walking players (the audit measured 1340).
Keyframes over 256 bytes are LZ4-compressed (`Compression::Lz4`, feature `lz4`, on by
default); a compressed packet that would not shrink is sent raw.

### Replicate once

`register_networked_ticked_component_once::<T>("name")` (or `_as(name, ReplicationClass)`)
marks a component that never changes after spawn: it travels in the record that
introduces the entity and in keyframes, never in a delta after that. `bevy_ticked::Owner`
is registered this way; **do** the same for your kind markers, spawn points and colours.
`ReplicationClass::Always` carries a component on every delta, changed or not, and is what
`SendRates::every::<T>(n)` divides.

### A drawn body drops what its authority dropped

An `Interpolated` entity's components are written from the authoritative record; they are
now also removed when the record stops carrying them. Under the fast path nothing else
would have removed them.

### A host keeps its world

The bridge used to despawn every tracked entity when a peer adopted either role. A joining
client still loses its solo world (the host's replaces it); a host keeps its own, and
`reset_on_host` raises the id counter over it and marks it alive from tick 0. A game that
relied on the host's menu-time world being cleared on hosting must clear it itself.

### Harness

`TickedNetwork::decode_messages::<M>` and `decode_messages_traced::<M>` decode any
ensemble message type out of the trace, with the frames it was sent and arrived. A test
that traces acknowledgements must switch tracing on before the session settles.

## T12 — lockstep part 2: the session on the tick, the stall, catching up

`AuthoritativeTick` gained two fields (`system`, `margins`), covered by the ensemble
protocol hash: rebuild every peer.

### The session's own actions are ruled onto a tick

**Before** a participant joined on an agreed tick and left on no tick at all: the host
stopped requiring their actions when their `LobbyClient` went, and every peer noticed on
whatever frame its roster changed, so a departed body could not be despawned
deterministically. **After** `AuthoritativeTick.system: Vec<SystemAction>` carries
`ParticipantJoined`, `ParticipantLeft`, `Pause(LockstepPauseReason)` and `Resume`, ruled by
the host into the next tick it simulates like an action. Every peer applies them inside
that tick (`LockstepSimulationSet::System`, before `::Game`): `LockstepRoster` changes and a
`TickedEvents<RosterChange>` entry is written on the same tick everywhere. A late joiner is
seeded from the roster messages for joins its snapshot already contained.

**Do** read `TickedEventReader<RosterChange>` inside the simulation to spawn and despawn
player bodies, and put your simulation systems in `LockstepSimulationSet::Game`. **Delete**
`spawn_players_at_agreed_tick`-style derivations from `LockstepLobbyParticipant`.

### Pause

`PauseLockstep(reason)` / `ResumeLockstep` on the host: the pause is ruled into the next
tick, the host holds `TickHoldReason::SessionPause` after running it and keeps nothing
flowing, clients apply the tick and wait on the next authoritative one; the resume is
ruled into the tick that lifts the hold. `LockstepPaused(Option<reason>)` on every peer says
so as of the ticks it has run. A client's own `Manual` hold composes with it: the host runs
the ticks that client scheduled before the pause and then waits on it.

### The stall, named and bounded

`LockstepStall { waiting_on, since, paused }` says who this peer is waiting on and for how
long (on the host, the clients whose actions the next tick needs; on a client, the host).
`StallPolicy { pause_after: 1 s, kick_after: Some(10 s) }`: past `pause_after` the stall is
reported as a pause; past `kick_after` the host despawns the participant's `LobbyClient`,
which rules a `ParticipantLeft` into the next tick, and the session runs again for the
survivors, who agree. Each waited-on peer is timed on its own.

### Catching up, and the buffer

A joiner with more ticks in hand than its buffer runs its clock up to fifty percent fast
(`TickRateDilation`) until the backlog is gone; the host never freezes for a join. A client
sizes `client_tick_buffer` from its own arrival margin, which the host measures as each
batch arrives and reports in `AuthoritativeTick.margins` (`OwnInputMargin`): the buffer
grows at once by the shortfall below `TARGET_ARRIVAL_MARGIN` (2) and shrinks a tick at a
time when comfortably early. The ping-based `AdaptiveTickBufferPlugin` remains the seed
before the first margin. `LockstepConfig::host_tick_buffer` defaults to 1, the smallest
grace window; the host pays no input lag of its own.

## T11 — the session pause

One new networked resource, `bevy_ticked::SessionPause`, covered by the handshake.

### A pause is the authority's word, replicated

**Before** there was no session pause. A host that alt-tabbed on the web got one frame a
second; every client kept ticking into a future the host had not produced, piled up two
seconds of lead and shed it at a couple of percent a second: the audit measured about 1900
ticks of excess lead, gone after twenty-five minutes. A game that wanted a pause menu had to
replicate it itself.
**After** `SessionPause(Option<Paused { at, reason }>)` is a networked ticked resource the
host writes. `PauseSession(reason)` / `ResumeSession` messages on the host take effect at
the next loop pass: the pause is stamped on the *next* tick (a snapshot for a tick a client
has already applied is dropped as stale, so a pause stamped on the current tick would never
arrive), the host runs that one tick, holds `TickHoldReason::SessionPause`, and keeps
broadcasting (the first held pass always sends). A client that receives it runs up to the
paused tick if behind, rolls back to it and forgets its prediction if ahead, and holds.
`ResumeSession` clears it; the host runs, and each client re-acquires its lead forward
through the "at or behind" path, no replay burst. The lead-taking jump now discards the
frame accumulator's backlog, so a two-second frame after a tab switch does not put the lead
sixteen ticks past target.

`PausePolicy` (resource, installed by both role plugins): `who_may_pause: HostOnly |
AnyParticipant` (a client's `PauseSession` becomes a `SendPauseRequest` the bridge carries as
`bevy_ticked/PauseRequest`; the host applies it as `PauseReason::Participant(uuid)` only
under `AnyParticipant`), `auto_pause_on_focus_loss: true` (`window` feature of
`bevy_ticked_networking`, default on: `WindowFocused` lost pauses as `HostUnfocused`,
regained resumes), `auto_pause_after_real_gap: Some(500 ms)` (a frame that long after the
previous one is a stall the host just came back from: it pauses as `HostStalled` at the tick
it is still on, so clients drop what they predicted, and resumes next frame),
`client_soft_hold_after: Some(250 ms)` (a client that has applied no snapshot for that long
holds `TickHoldReason::SoftHold` rather than run ahead of a host that may be gone; the next
snapshot releases it).

**Delete** the game's own "host lost focus, tear down the lobby" handling and any
replicated pause flag. **Watch** a unit test that runs a client for many frames without a
snapshot stream: it soft-holds after a quarter second; set
`PausePolicy { client_soft_hold_after: None, .. }` if the test is about something else.

`a_host_alt_tab_auto_pauses_and_no_lead_piles_up` is un-ignored: after a two-second host
freeze the client is within a tick or two of its target lead one second later.

## T10 — existence history, spawn/despawn rollback, deterministic ids

**Ids changed shape** (`sequence << 8 | slot`) and the id allocator is on the wire as
`bevy_ticked::TrackedIdAllocator`, so every peer rebuilds; the handshake refuses a peer from
before.

### Existence is in the history

**Before** a rewind past a spawn left a husk (still tracked, every registered component
stripped, captured forever) and a rewind past a despawn could not bring the entity back. A
client's snapshot despawned anything it did not name, which made a predicted spawn
impossible: the next snapshot deleted it.
**After** `TrackedEntityLifetimes` records when each tracked id was born and died, driven
by the same capture, restore, truncate and prune calls as the component histories.
`restore_all` tombstones what did not exist at the target tick, revives what did, and
rebuilds from the histories what a plain despawn destroyed. `apply_full_body` tombstones an
absent id only if it was born at or before the snapshot's tick; an id spawned after is left
to the rollback, which knows whether the replay spawns it again. There are no husks.

### `despawn_ticked`

```rust
// Before
commands.entity(bullet).despawn();
// After
commands.entity(bullet).despawn_ticked();   // TickedEntityCommandsExt, also on EntityWorldMut
```

A tombstone: the entity and its children are `Disabled` (every query, capture and snapshot
skips them), it is unindexed, and it is kept until the history window has passed its death.
A rewind to a tick it was alive at revives it intact, same `Entity`, children, observers and
local-only state. A plain `despawn` on a tracked entity still works: an observer records the
death, a rewind past it rebuilds the entity through the spawn path (re-dressed by the game's
`On<Add, TickTrackedEntity>` observer) with local-only state lost, and it warns once. The
clippy `disallowed-methods` snippet in `docs/ROLLBACK_RULES.md` catches the rest.

### Ids carry a slot; clients mint

`TickTrackedEntity::new(SpawnerSlot, sequence)`, `.slot()`, `.sequence()`, `SLOT_BITS = 8`.
The authority is slot `0`; the host gives each client a slot `1..=255` in the welcome
(`LocalSpawnerSlot`, a core resource; the host holds slot 0). Two peers minting in the same
tick cannot collide, so a client predicts a spawn — a bullet leaving its own gun — and the
host, running the same simulation from the relayed input, mints the *same id* and confirms
it, onto the same entity.

`TickTrackedEntityCounter` is gone. `TrackedIdAllocator` (`next(slot)`, `next_authority()`,
`raise_to(id)`, `peek(slot)`) is a ticked resource (rolled back, so a replay re-mints the same
ids) and a networked one (the authority's snapshot corrects a client's slot-0 sequence).
Spawn with `TrackedSpawner` (`spawn(bundle)` under the local slot, `spawn_by(slot, bundle)`
under the shooter's) or `TrackedWorldExt::{spawn_tracked, spawn_tracked_by}`; a mint whose id
has a tombstone revives it. `HealthWarnings.client_minted_tracked_id` now means a client
minted under the authority's slot.

**Delete** the game's `spawn_tracked`/`LocalPlayerUuid` authority module, its host-only
gate around bullet spawns, its `debug_assert!` on the counter, and every `Explosion`-style
tracked entity that existed only to fire a sound on every peer (a `TickedEvent` does that).
**Watch** a test that asserted sequential ids (`[1, 2, 3]`): the authority's are now
`[256, 512, 768]`; spell them with `TickTrackedEntity::new(SpawnerSlot::AUTHORITY, n)`.

### The examples and the harness

Both examples now add `TickedEnsembleSessionPlugin::default()` and drop their hand-rolled
`on_lobby_ready`: the session plugin adopts the roles, runs the registry handshake and hands
each client a slot. Player bodies carry a networked `PlayerSlot(u8)` set by the host from
`SpawnerSlots` (0 for itself); a body is spawned only once its player's slot is known.
Bullets are spawned by every peer inside the simulation with
`world.spawn_tracked_by(SpawnerSlot(shooter's slot), ..)` in uuid order, so the shooter
sees its bullet the frame it fires and the host confirms it under the same id; the host gate
is gone. Expired and hit bullets use `despawn_ticked`.

The harness fixture gained `Input::FIRE`, `EntityKind::PELLET`, a `PlayerSlot` on every body
(set by `seat_everyone` from each client's `LocalSpawnerSlot`), a `Fuse` and a pellet spawned
under the shooter's slot; `view::tracked_entity_count` and `view::tombstone_count`. The
bridge suite `lifecycle_rollback.rs` is run-2d's grenade tests over it: a predicted bullet
survives the snapshot that predates it, a mispredicted spawn is tombstoned when the authority
never confirms it, a predicted despawn the host contradicts is undone, two clients firing in
the same tick never collide, a client never mints an id the host will reuse, `On<Add>` fires
once for a confirmed prediction, a spawn survives a lossy link, and a grenade does not blink.

## T9 — the misprediction fast path, send rate, bounded replay

Nothing on the wire changed.

### A snapshot that agrees with the prediction costs nothing

**Before** a client rolled back and replayed its whole lead on every snapshot: seven
simulation runs per frame on a world where nothing had happened, every `Changed<T>` and
observer firing seven times for it, and an unregistered counter advancing seven times per
tick. **After** the client compares the packet with what it predicted for the packet's tick
— every predicted entity's networked components, by encoded bytes, plus the set of tracked
ids and the networked resources — and when they agree it records the packet, files the
relayed inputs, observes its margin and moves on. `ReplayStats.skipped_identical` counts
those. Interpolated entities are not compared: they are never predicted.

The comparison is exact and on the bytes. There is no `PartialEq` bound: a foreign component
(a character controller's state from another crate) that never implemented equality is
still networkable, and a type whose encoding is not canonical for equal values is replayed
rather than mis-skipped.

**Delete** a game's own "did anything change" check around its rollback, and any
`Changed<T>`-driven presentation that was rate-limited to survive the replays. **Watch** a
test that delivered a *matching* snapshot to force a replay: it forces nothing now; deliver a
packet that differs.

### `TickedServerPlugin::send_every`

`TickedServerPlugin::new().send_every(n)` broadcasts every `n`th tick (`SendEvery` resource).
With the fast path a snapshot costs an agreeing client nothing, so the rate is a bandwidth
knob only; the welcome carries it and a client draws remote bodies `2 * send_every` ticks
behind.

### A replay is bounded per frame

A correction from far back (a burst of stale snapshots after a stall) replays at most
`MaxTicksPerFrame` ticks in a frame; the rest carries over under `AwaitingReplay`, with the
clock held by `TickHoldReason::Replaying` (new) until the world is caught up. A new snapshot
supersedes a replay in progress.

## T8 — remote entity modes, hold-last input, correction smoothing

Nothing on the wire changed except one new networked component, `bevy_ticked::Owner`, which
the registry handshake covers.

### Entities the client does not control are interpolated

**Before** a client simulated every tracked entity through its replay with whatever input it
had, which for a remote player was nothing: a walking body stood still for the whole lead on
every client and snapped to the next snapshot, sixty-four times a second. Every game wrote a
smoothing layer over it.
**After** `ReplicationMode { Predicted, #[default] Interpolated }`, a client-side component.
An entity with no marker is interpolated: every loop pass after the snapshot, its networked
components are set to the authoritative record for `latest applied tick - InterpolationDelay`
(default 2 ticks), and `TickedInterpolation` blends between consecutive authoritative states.
The display tick (`DisplayTick`) advances one tick per tick toward that target and catches up
only when more than two behind, so a bunch of late snapshots does not move a body three ticks
in one frame; a snapshot that arrives too late for the rollback, or is superseded by a newer
one before it is applied, is still recorded in `AuthoritativeHistory`, so the drawn path has
no hole where it was. On a `bad_wifi` link a walking body is drawn moving at most two units
per tick, where the audit saw it jump by the whole lead.
It is a little behind, always smooth, and exactly where the host said. Predicted entities are
simulated through the replay as before.

`Owner(pub u128)` is now the stack's networked component (`"bevy_ticked::Owner"`, registered
by both role plugins). `RemoteInterpolationPlugin` (installed by `TickedClientPlugin`) marks an
entity with the local player's `Owner` as `Predicted` when it appears and when the role
arrives. `AuthoritativeHistory` keeps the last 64 snapshots' records; the misprediction check
and the delta phase read it too.

**Delete** the game's `OwnerPlayer`/`PlayerUuid` component and its registration, and every
"remote body with no input" workaround. **Do** make remote physics bodies kinematic on
clients (the examples do it in an observer on `Owner`): the host owns their motion, and a
dynamic body fights the restore every tick. **Watch** a test that spawns a tracked entity on a
client without an `Owner` and expects a snapshot's value to be *visible*: it is interpolated
now, two ticks behind; give it `ReplicationMode::Predicted` if the test is about prediction.

### Hold the last input

`InputQueue::get_or_last(tick, uuid)` and `at_tick_or_last(tick)`: a player with no input for
a tick keeps pressing what they last pressed. The server relays other players' inputs in
`FullBody.inputs_ahead` (T7), so a predicted remote body has real inputs to hold. **Delete**
the game's `NetInput` hold-last half and `PlayerInputState`.

### Correction smoothing

`TickedSmoothingPlugin` + `CorrectionSmoothing { decay_rate: 12, max_offset: 2, max_angle: 1,
apply_to: SmoothingTarget::{Self_, Child(Entity)} }` on an entity: a snapshot correction moves
the simulation whole and the renderer by a decaying offset, applied in `PostUpdate` after the
tick blend and undone in `TickedSystems::Restore`, or written to a visual child and never to
the simulated transform. The local player's entities are exempt (a correction to what you
control should be felt), so is anything with `NoCorrectionSmoothing`, and so is the initial
sync; a correction beyond `max_offset`/`max_angle` is shown as the jump it is.
`CorrectionStats` counts them. **Delete** `rollback_smoothing.rs`, `smoothing.rs` and their
`CorrectionStats`.

`measure_prediction::<T>(app, distance)` records `PredictionError`: the distance between what
the client had captured for a snapshot's tick and what the authority sent, per snapshot.
`ClientSet::{BeforeSnapshot, ApplySnapshot, AfterSnapshot}` inside `PreTick` is where a game
hooks its own before/after measurement.

### The examples

The core half of this phase (`ReplicationMode`, `Owner`, `RemoteInterpolationPlugin`,
`TickedSmoothingPlugin`, hold-last input, `ClientSet`) is in the T8 section of
`docs/MIGRATION.md`. This covers what a game built like the examples has to change, what the
harness gained, and the tests that came with it. Nothing on the wire changed beyond `Owner`,
which every peer now registers, so both peers must still be built from the same commit.

## The examples

#### `PlayerUuid` is `Owner`

**Before** each example carried its own `PlayerUuid(u128)` component, registered under
`"PlayerUuid"`, and used it for the three things every game used one for: which body gets the
camera and the input, which body is the local player's, and which body a snapshot's absence
rule may not touch.

**After** `bevy_ticked_networking::replication::Owner(u128)`, in the prelude, registered by
the role plugins under `"bevy_ticked::Owner"`. The stack reads it to decide which bodies a
client predicts. **Delete** the game's component and its registration; the two examples did
nothing else.

```rust
// Before
struct PlayerUuid(u128);
.register_networked_ticked_component::<PlayerUuid>("PlayerUuid")
commands.spawn((tracked_id, EntityKind::Player, .., PlayerUuid(participant.player_uuid)));
// After
commands.spawn((tracked_id, EntityKind::Player, .., Owner(participant.player_uuid)));
```

#### A remote body is interpolated and takes no input

**Before** `apply_inputs` ran on every player body with whatever `InputQueue::at_tick` held,
which for a remote player was nothing past the relayed inputs; the body stood still through
the replay and snapped forward at the next snapshot. Neither example had a written-out
"zero input for non-local players" branch — the stall *was* the workaround, by omission.

**After** `apply_inputs` reads `at_tick_or_last(tick)` (a player with no input for this tick
is still pressing what they last pressed) and drives only the bodies this peer simulates:

```rust
fn drives(local_client: Option<&LocalClientPlayer>, mode: Option<&ReplicationMode>) -> bool {
    local_client.is_none() || matches!(mode, Some(ReplicationMode::Predicted))
}
```

On the host that is everyone; on a client it is the local player's body (the stack marks it
`Predicted` from `Owner`) and anything the game marks so. Everything else is put at the
authority's state after each snapshot by the stack and needs no input. Shooting still reads
`at_tick`: a held trigger from a stale input would spawn bullets the host never did.

#### A remote physics body is kinematic on a client

The stack restores an interpolated entity's networked components after every snapshot, and
the simulation still runs on it between restores. A dynamic avian body fights that restore
every tick — damping, colliding, falling, integrating from a velocity the host has since
changed. Both examples choose the body kind where the owner is known:

```rust
fn body_kind(local_client: Option<&LocalClientPlayer>, owner: Option<&Owner>,
             mode: Option<&ReplicationMode>) -> RigidBody {
    let Some(local) = local_client else { return RigidBody::Dynamic };   // the host
    let mine = owner.is_some_and(|owner| owner.0 == local.0);
    if mine || matches!(mode, Some(ReplicationMode::Predicted)) { RigidBody::Dynamic }
    else { RigidBody::Kinematic }
}
```

It is called from the `On<Add, TickTrackedEntity>` observer, not `On<Add, Owner>`:
`apply_full_body` inserts `TickTrackedEntity` **last**, after every networked component, so
that observer is the one that sees the owner and runs after anything keyed on `Owner`. A
second observer, `On<Insert, ReplicationMode>`, changes the body when a game changes the mode
later. In `fps_shooter` the kind overrides the `RigidBody::Dynamic` inside elan's
`character_controller_bundle()`; the rest of the bundle stays, so a predicting client still
has a controller to predict with.

#### Interpolation and smoothing

Both examples add `TickedInterpolationPlugin` and `TickedSmoothingPlugin`, and put
`TickedInterpolation::default()` and `CorrectionSmoothing` on every player body (the local
player's is exempt by `Owner`, nothing to mark). `top_down_shooter` raises `max_offset` to
four player radii: the default two units is two pixels there, and every correction would have
been "too big to smooth". `fps_shooter` keeps the default: two metres is a respawn.

Bullets get `TickedInterpolation` too, and their `Position -> Transform` copy moved from
`Update` into the tick (`sync_bullet_transforms`, after `bullet_collision`), so the
interpolation records one state per tick. `top_down_shooter::sync_visuals` no longer writes
any translation — avian writes the player's inside the tick and the blend would only have
been overwritten and put back — and rotates the laser child and the bullet sprite as before.
`fps_shooter::sync_visuals` is gone; it only did the bullet copy.

## The harness (`bevy_ticked_testing`)

- `fixtures::minimal::Owner` is a re-export of the stack's `Owner` and is no longer
  registered by `register_components`. `apply_inputs` holds the last input
  (`at_tick_or_last`): a body whose owner sent nothing for this tick keeps its velocity, as
  it already did by having `Vel` be state, but a stale input is now re-applied rather than
  skipped. No existing test's counts changed.
- `fixtures::minimal::install_with_transform(app)`: `install` plus a `Transform` on every
  tracked entity (attached by an observer, so a body a snapshot spawns has one), written from
  `Pos` each tick by `sync_transform`, blended by `TickedInterpolationPlugin`. `Transform` is
  registered for rollback under `"Transform"`, never the wire. Opt-in: the integer fixture is
  exact and a float transform is not. `sync_transform` re-attaches a transform a rollback
  restore stripped — a restore removes a rollback-only component from every entity the
  restored tick has no record of, and a snapshot-spawned body has none for the ticks before
  its first capture.
- `fixtures::minimal::spawn_player_with_transform(world, uuid)`: `spawn_player` with the
  transform and interpolation state already on the body. `seat_everyone` is unchanged; under
  `install_with_transform` the observer gives its bodies the same.
- `view::authoritative_tick(app) -> Option<u64>`: the newest tick in `AuthoritativeHistory`,
  what interpolated entities are drawn from, `InterpolationDelay` behind.
- `view::replication_mode(app, id) -> Option<ReplicationMode>`: `None` is "interpolated by
  default" as well as "no such entity".
- `tests/bites.rs::assert_all_peers_agree_fails_on_a_corrupted_replica` corrupts the client's
  **own** body now. It used to corrupt the host's, which on a client is interpolated and is put
  back from the authoritative record every tick; the harness then had nothing to bite on.

## Tests that came with it

`crates/bevy_ticked_networking_ensemble/tests/remote_modes.rs`, host and two clients: B's copy
of a walking A never goes backwards and ends about the delay behind the host; on B, A's body is
exactly the host's record for `authoritative tick - delay` after every restore (a probe in
`PreTick` after `ClientSet::AfterSnapshot`) and within one tick of it after every frame; with
transforms, the drawn body moves one unit a tick at most and lags by the delay; a body marked
`Predicted` on B moves once per tick of B's clock through every replay and runs ahead of the
host; snapshots to B carry A's inputs for ticks after their own, and B holds them to the
snapshot tick plus A's margin; a correction of the local player's body never gets a
`SmoothingOffset` and is not counted, while the same correction of a predicted remote body
is smoothed; the offset shrinks every frame, takes the frames its decay rate says, and never
snaps; and a hundred frames of A walking on congested wifi never move the drawn body by
anything like the lead. `get_or_last` has a unit test in the same file.

#### What the wifi test does not say

It was asked to assert "never more than two units a frame" and asserts a ceiling of six, under
half the lead, and no more than twenty frames of a hundred over two. On `Link::bad_wifi()` two
things still bunch the drawn steps, both in `RemoteInterpolationPlugin` and not in the
examples or the harness:

- forty milliseconds of jitter lands two to five snapshots in one frame, and the display tick
  follows the applied tick one for one, so the body takes that many steps at once;
- a snapshot overtaken on the link is dropped as stale *and not recorded* in
  `AuthoritativeHistory`, so the history has holes and "the newest record at or before the
  display tick" jumps by the hole when the next record lands.

Measured over six seeds and three hundred frames each: six frames in ten the drawn body does
not move, most of the rest it moves two, and it moves four or five about one frame in twenty.
A display clock that advances one tick per tick with the delay absorbing the burst, and a late
snapshot recorded even when it is not applied, would close both and let the test say two.

#### A harness gotcha the smoothing tests found

`drop_next_packets` takes effect at the send. A snapshot already on a 15 ms link lands on the
next frame regardless, and puts a corruption right before it ever reaches the transform — so a
test that corrupts a body to force a visible correction drops first, runs two frames to drain
the link, then corrupts, then runs two more for the wrong value to be drawn. `corrupt_visibly`
in `remote_modes.rs` is that, and asserts the transform actually went wrong.

## T7 — snapshot wire v2

**This is a wire-format change**, the one this crate makes in the overhaul. Every peer of a
session must be built from this commit or later; the registry handshake refuses anything
else and names the first registration that differs. `PROTOCOL_VERSION` is `2` and is folded
into every registry hash.

### Wire indices are derived from names; registration order is not a format

**Before** a networked type's index was its position in registration order, assigned as a
`u16` and sent in every snapshot. Reordering two registrations made a peer read one type's
bytes as another's with no error of any kind; the only protection was a hand-kept rule.
**After** the index is the type's rank among every networked wire name, sorted. Two peers
that register the same names in any order agree about every byte. A rollback-only type has
no wire index and cannot shift anything. The order is computed the first time anything reads
it (a snapshot, a handshake, `wire_hash()`); registering after that panics, so register in a
plugin's `build`.

### A networked registration needs a name

```rust
// Before
app.register_networked_ticked_component::<Health>();            // gone
app.register_networked_ticked_component_as::<Health>("Health"); // gone
app.register_networked_ticked_resource::<Round>();              // gone
// After
app.register_networked_ticked_component::<Health>("Health");
app.register_networked_ticked_resource::<Round>("Round");
```

The name is the type's identity on the wire. Two types with the same name panic at
registration. Renaming the Rust type is free; changing the string is a wire break. Rollback-
only registrations (`register_ticked_component`, `_as`) are unchanged and never on the wire.

**Delete** a game's index assertions (`assert_eq!(registry.index_of::<Pos>(), Some(3))`) and
its "append only, never reorder" comments. **Watch** `TickedComponentRegistry::index_of` still
exists and is the registration index, meaningful in this process only; the one that travels
is `wire_index_of`.

### The snapshot is entity-major, sorted and addressed

`WorldSnapshot` is gone. A `SnapshotPacket { seq, tick, your_margin, body }` carries a
`SnapshotBody::Full(FullBody)` — one `EntityRecord { id, present: TypeMask, bytes }` per
tracked entity, sorted by id, its components concatenated in wire-index order with no length
prefixes, then `(wire index, bytes)` resources, then `inputs_ahead` — or a `Delta`, reserved
for the delta phase and dropped (counted in `ReplayStats.dropped_delta_body`) until then.
The same world encodes to the same bytes twice. Two walking players cost 1340 bytes a tick
before and 52 now (host to each client, everything on the link).

Each client gets its own packet: `seq` counts per recipient and `your_margin` is that
client's own input-arrival margin (`InputMargins` no longer travels to everyone).
`SendNetworkSnapshot { recipient: Option<u128>, bytes }` is one encoded packet per recipient;
`SnapshotRecipientList(Vec<u128>)` (maintained by the transport, the verified clients)
replaces `SnapshotRecipients(usize)`; absent means one unaddressed packet, empty means
nothing is built. `SnapshotStats` counts bytes and `oversize` (over 1200 bytes, warned
once) on the server itself.

`build_snapshot`/`apply_snapshot` are `build_full_body`/`apply_full_body`, the latter
returning `Applied { spawned, despawned, undecodable, duplicate_ids }`. A duplicate id in a
body is applied once and counted in `HealthWarnings.duplicate_ids_in_snapshot`. An entity
stripped of every networked component now survives bare on every peer; the old type-major
shape derived existence from the union of the component maps and despawned it.

### Acks and relayed inputs

`NetworkInputPayload.ack: Option<u32>` carries the newest snapshot `seq` the client applied;
the transport triggers `ReceivedSnapshotAck { sender, seq }` and the server keeps `LastAck`.
Nothing reads it until the delta phase. `FullBody.inputs_ahead` carries other players'
inputs the host already holds for ticks after the snapshot's; a client files them in its
`InputQueue` (its own are ignored), so a replay uses what those players pressed rather than
nothing.

### The ensemble bridge and the harness

The transport half of the wire phase. The core and `bevy_ticked_networking` half (name-derived
wire indices, the entity-major packet, per-recipient packets, acks) is in the T7 section
proper; this covers what changed in `bevy_ticked_networking_ensemble` and
`bevy_ticked_testing`, and what a game on the bridge has to change. Both peers must be rebuilt:
the snapshot message, the handshake and the input message all changed on the wire.

#### Registrations are named, and the name is required

**Before** `register_networked_ticked_component::<T>()` took its wire name from
`std::any::type_name`, and `_as::<T>("Name")` was the opt-in; the same for resources.

**After** `register_networked_ticked_component::<T>("Name")` and
`register_networked_ticked_resource::<R>("Name")`. There is no unnamed variant and no `_as`.
`type_name` is explicitly unstable across compiler versions, and a wire index derived from an
unstable string is a session that breaks on a rustc upgrade.

```rust
// Before
.register_networked_ticked_component::<Position>()
.register_networked_ticked_component_as::<Health>("Health")
// After
.register_networked_ticked_component::<Position>("avian::Position")
.register_networked_ticked_component::<Health>("Health")
```

**What to change** Give every networked registration a short, stable string; the examples use
`"avian::Position"`, `"elan::LastJump"`, `"EntityKind"`. Renaming the Rust type is free;
changing the string is a wire break, so pick once. Registration *order* no longer matters and
the "order must match on all peers" comments can go.

#### `TickedEnsembleSessionPlugin` is a struct

**Before** a unit struct: `app.add_plugins(TickedEnsembleSessionPlugin)`.

**After** `TickedEnsembleSessionPlugin { handshake_timeout: Duration }`, `Default` is five
seconds: `app.add_plugins(TickedEnsembleSessionPlugin::default())`. The field is also a
resource, `HandshakeTimeout(Duration)`, so a test can shorten it on one peer.

#### The registry handshake gates the world (F19)

**Before** both peers announced a hash a frame or two after the lobby appeared, compared it,
and tore the session down on a mismatch — after whatever snapshots had arrived in between were
applied, which is a world built from bytes that mean something else and then despawned. A host
with three matching clients and one mismatched one dropped its own role and ended the game for
everybody.

**After**

- Each peer announces `TickedRegistryHandshake { components, resources, component_names,
  resource_names }` — the two hashes and the two sorted name lists — when `bevy_ensemble` marks
  the counterpart `HandshakeVerified`, that is, once the transport's own protocol check has
  passed. Control message `bevy_ticked/RegistryHandshake`, authority `Any` (both sides
  announce), reliable.
- A host that matches a client inserts `TickedPeerVerified` on that `LobbyClient`, adds its
  uuid to the server's `SnapshotRecipientList`, and sends `TickedSessionWelcome { slot,
  server_tick, send_every }` (`bevy_ticked/SessionWelcome`, host-only, reliable). Slots come
  from `SpawnerSlots` on the host, `1..=255`, lowest free first, freed when the `LobbyClient`
  goes; the client keeps its in `LocalSpawnerSlot(u8)`. Nothing uses the slot yet; it is for
  the predicted-spawn phase.
- A client that matches its host inserts `RegistryVerified`. **Until it is present the bridge
  drops every snapshot** and counts it in `ReplayStats.dropped_before_handshake`. The host
  only sends to verified clients, so in practice the count is the odd packet that crossed a
  slow link before the handshake did.
- A mismatch names the first differing registration, in `RegistryMismatch.difference` and in
  the log: `component "Health" is registered on this build and not on the peer's` or `the
  peer registers "WeaponState" which this build does not`; resources after components. On a
  client the role is dropped and the latch blocks re-adoption until the lobby goes, as before.
  **On a host the session continues:** the mismatched client is refused (never verified, never
  sent a snapshot) and the error says why; the client finds out on its own side, since both
  compare.
- A client that has held its role for `HandshakeTimeout` without hearing its host's registries
  latches `HandshakeTimedOut { waited }`, drops the role and releases `AwaitingSync`, with an
  error saying the host is on a build without the handshake or the link never delivered it.
  Before, it sat paused for ever.

**What to change** Nothing, on the session plugin: it is all inside. A game that adopts roles
by hand and runs only `TickedNetworkingEnsemblePlugin` has no handshake and no gate, exactly as
before — the bridge only gates when the session plugin installed the handshake — and should
move to the session plugin to get F19's protection. A game that showed `RegistryMismatch` to
the player can show `difference` instead of two hashes. `RegistryMismatch` is `Clone` and no
longer `Copy` (it carries the name lists).

#### `TickedSessionLobby(Entity)`

New resource, present while a `Lobby` entity exists (host or client) and removed with it.
The bridge's own systems read it instead of `Single<Entity, With<Lobby>>`; a game that sends
its own lobby messages can too. Installed by either plugin of the crate.

#### Snapshots are addressed, and the bridge no longer counts bytes

**Before** `SendNetworkSnapshot(WorldSnapshot)` was broadcast to the lobby, encoded by the
bridge (a second postcard pass, to fill `SnapshotStats.bytes`), and `SnapshotRecipients(usize)`
told the server how many copies that was.

**After** `SendNetworkSnapshot { recipient: Option<u128>, bytes }` arrives one per verified
client, already encoded; the bridge sends `EnsembleSnapshotMessage { bytes }` unreliably to
that client's `LobbyClient` entity (or, with `recipient: None`, to the lobby, which is the
broadcast a bridge without the session plugin still gets). `SnapshotRecipientList(Vec<u128>)`
replaces `SnapshotRecipients`; the session plugin maintains it from `TickedPeerVerified`.
`SnapshotStats.bytes`, `max_bytes`, `last_bytes` and the new `oversize` are counted by the
server at the encode. A packet that does not decode is dropped with one warning; the fuzz
tests feed the bridge garbage and it must never panic.

`EnsembleInputMessage<T>` carries `NetworkInputPayload { inputs, ack }`; on the host the bridge
triggers `ReceivedSnapshotAck { sender, seq }` after the packet's inputs.

**What to change** A game that read `SnapshotRecipients` reads `SnapshotRecipientList`; a
game that built `EnsembleSnapshotMessage { payload }` by hand (a test crafting a packet) builds
`{ bytes: encode_packet(&packet) }`.

#### The harness

- `fixtures::minimal` registers under `"Pos"`, `"Vel"`, `"EntityKind"`, `"Owner"` with the new
  API; a test that registered a subset by hand uses `register_networked_ticked_component`.
- `wire::assert_wire_order` / `assert_resource_wire_order` compare the **sorted** wire names
  against `expected` as a set; a reorder no longer fails them, an add/remove/rename still
  does. New: `assert_wire_names(app, &[..])`, `assert_resource_wire_names(app, &[..])`,
  `wire_hash(app) -> u64`, `resource_wire_hash(app) -> u64`.
- `TickedNetwork::decode_snapshots(from, to) -> Vec<SnapshotPacket>`: every traced snapshot
  from one peer to another, unframed and decoded, in send order — the way to read what a
  client was *told* (`seq`, `your_margin`, the body) rather than what it has since predicted.
  `snapshots_sent` and `snapshot_packets` are unchanged.
- `assert_bandwidth_within` returns the measured bytes per tick, so a test can print the figure
  it budgets against. With the minimal fixture, a host and two walking clients over a cable,
  it measures **52 bytes per tick** from the host to each client, everything on the link
  included (the audit's figure under the old shape was 1340); a snapshot packet alone is 51
  bytes mean, 59 max.

#### Tests that came with it

`crates/bevy_ticked_networking_ensemble/tests/wire_v2.rs`: nothing is applied before the
handshake matches (a crafted early snapshot is dropped and counted); a mismatched client never
holds a tracked entity and is told which registration differs; a client whose announcement
never arrives is refused after the timeout and not left paused; each client's packets carry
its own margin and its own sequence numbers; a departed player's margin and recipient entry go
with it; a verified client is welcomed with a slot that is freed on leave and reissued on
rejoin; snapshots go to verified clients only (not a mismatched one, not a pending one, and to
a promoted one once promoted); and the bytes-per-tick budget above.

`lossy_links::a_reordered_snapshot_is_dropped_over_the_link` was passing by accident: on a
cable the loopback delivers every unreliable packet on the next frame, so there is never a
second packet in flight to swap with, and the one stale drop it counted was the duplicate
tick-1 snapshot every session starts with — which lands before or after the test's counter
reset depending on the wall-clock ping the client seeds its lead from, and so on machine load.
It now runs over a link with three frames of delay, where an overtake is possible and does
happen.

## T6 — lockstep part 1: join, checksum, late actions, trust

One wire change: `ClientLoaded` carries the joiner's tick buffer. Both peers must be rebuilt;
`bevy_ensemble`'s protocol hash covers it, so a stale peer is refused at the handshake rather
than joined and hung.

### The join no longer depends on the two buffers agreeing (F23)

**Before** the host sized a joiner's grace window from `host_tick_buffer` alone, and the joiner
started its scheduled sequence wherever `client_tick_buffer` put it. When the joiner's buffer
was the larger — the adaptive tuner grows it on a bad link, and a client that had played on
satellite and then joined a LAN host carried it in — the joiner's first scheduled tick landed
after the first tick the host required of it, and the host waited for the ticks in between for
ever.

**After** `ClientLoaded { buffer }` tells the host what the joiner schedules with, and
`joined_at_tick = current + 1 + max(host_tick_buffer, joiner's buffer)`. On the joiner,
`LastScheduledTick` is set to the snapshot's tick once the game has applied it, so the first
flush fills forward from there; the host drops the batches for ticks it has already simulated.
When a lobby goes, `LockstepConfig` goes back to what the plugin was built with
(`InitialLockstepConfig` holds it) and `AdaptiveBufferState` is reset, so a buffer grown on one
link is not carried into a session on another.

**What to change** Nothing in a game that used `LockstepPlugin` as documented. A game that
constructed `ClientLoaded` itself gives it the buffer. A game that constructed `LockstepConfig`
by field adds `..default()` (it has a new field, below).

### The host has no input lag, and never waits on itself (F29)

**Before** the host scheduled its own actions `host_tick_buffer` ahead, as though it were a
client of itself, and its pause check required its own entry — which the flush that would have
inserted it does not run while held. With `host_tick_buffer: 0` that was a session that never
started.

**After** the host's actions go into the tick about to run; the authoritative broadcast carries
them to every client with that tick. `host_tick_buffer` is what it always effectively was: the
grace window a client's actions get after its `joined_at_tick`. It is clamped to at least one
at build, with a warning. The host's own participant is never in the set the pause check or the
broadcast waits on. A peer with no `LocalMultiplayerPlayerId` drops its local actions with a
`warn_once!` rather than filing them under uuid 0, a player no roster contains.
`AdaptiveBufferTuning::min_buffer` (default 4, never below 1) replaces the private constant.

**What to change** A game that read `host_tick_buffer` as "the host's input lag" reads it as
the grace window. A game that measured host-side input latency measures it again.

### The checksum log follows the session (F24)

**Before** a peer's `ChecksumLog` sampled whenever it ticked, lobby or not, and kept what it
had across a join. A player who idled in a menu and then joined had samples at tick numbers
the host was also at, of a different world, and the exchange reported a desync at the first tick
both logs shared.

**After** `ChecksumLog::clear()` (new, core crate) is called when a join snapshot arrives and
when the lobby goes; the exchange's parked reports go with it. `JoinSnapshotReceived` (local,
never on the wire) is the untyped message that carries "the world is about to be replaced" to
parts of the crate that do not know the game's snapshot type; the exchange reads it after
`LockstepJoinSet::ApplyJoinSnapshot` and before the finalize set flips the client to ready.
`bevy_ticked_lockstep_networking::ChecksumLogPlugin` is now the lockstep crate's own: the core
sampler gated on `With<Lobby>`, with the same `in_set`. It samples nothing while there is no
lobby unless built with `.sample_without_lobby()`.

```rust
// Before: the core plugin, via the lockstep crate's re-export
app.add_plugins(ChecksumLogPlugin::<MyHash>::default());
// After: the same line, now the lockstep sampler. To keep sampling solo:
app.add_plugins(ChecksumLogPlugin::<MyHash>::default().sample_without_lobby());
```

**What to change** A game that relied on solo samples — a determinism test comparing two solo
runs — adds `.sample_without_lobby()`, or names `bevy_ticked::checksum::ChecksumLogPlugin`
explicitly. A game that imports both preludes and names `ChecksumLogPlugin` unqualified now has
two: import the one it means. `bevy_ticked_testing`'s lockstep peers use the core plugin and are
unaffected.

### Late and far-future client actions (F25)

**Before** `receive_client_actions` merged whatever a client scheduled, for whatever tick. An
action for a tick the host had already simulated changed the host's record of it: the clients
had applied the tick without it, the next client to join was caught up *with* it from the
tracker, and the two disagreed from then on. This happens on every join — the joiner's first
flush now fills from its snapshot tick, and the host is past that by the time the batch lands.
An action for a tick a million ahead was a tracker entry kept for a million ticks. A client
that requested a snapshot and went quiet pinned the tracker's floor for the rest of the session.

**After** the host drops a batch for `tick <= current` and for `tick > current +
action_horizon`, with a `debug!` each. `LockstepConfig::action_horizon` is new, default 128.
`PendingClientJoins` forgets a uuid when its `LobbyClient` is removed
(`forget_departed_client_joins`, an observer on `Remove`) and when it has been pending longer
than `PENDING_JOIN_WINDOW_TICKS` (4096) — the tracker's keep-floor is bounded by that.

**What to change** `LockstepConfig { .. }` literals add `..default()`. A game that read
`PendingClientJoins` to keep its own catch-up state reads it knowing entries can leave.

### Trust (F27)

**Before** every message from a client was taken at face value.

**After**
- A `JoinSnapshotRequest` from a uuid that is already a participant is ignored; a repeat from
  the same uuid within `JOIN_SNAPSHOT_REQUEST_INTERVAL` (1 s, on the frame clock) is answered
  by the capture already under way. `LastJoinSnapshotRequests` is the bookkeeping.
- A `ClientLoaded` from a uuid that is already a participant is ignored: it used to re-issue
  `joined_at_tick`, which reopened the grace window in which the sender's missing actions
  count as empty. The three systems that read `ClientLoaded` off the wire now read
  `ClientAccepted` (local, never on the wire), written once per accepted join by
  `activate_loaded_client_participants`.
- `ClientScheduledActions` and `ChecksumReport` are accepted only from
  `LockstepLobbyParticipant`s of the local lobby.
- On the host a `ChecksumReport` latches `Desync` only against the host's own sample at that
  tick; a report for a tick the host has no sample for is dropped, not parked. A client still
  parks, because it is behind the host by construction.

**What to change** Nothing, unless a game sent one of these itself. A game whose clients sent
`ClientLoaded` more than once — some did, on every snapshot — sees the repeats logged at
`debug` and ignored.

### The roster is applied before the tick loop

`apply_received_participants` and `apply_pending_lockstep_participants` moved from `Update` to
`PreUpdate`, after `EnsembleSet::ReceivePackets`. A participant is on the roster before the
first tick of the frame its `ParticipantJoined` arrived in, which is what lets a game spawn the
player *inside the tick* at `joined_at_tick` and have every peer do it on the same tick. A game
that ordered against those systems in `Update` removes the ordering.

### `block_placer` (F28)

The example spawns players inside `TickedSimulation` from the roster's `joined_at_tick`
(`spawn_joined_players`), sorted by uuid, rather than from an `Update` observer on the frame
the roster arrived — which was a different tick on every peer. Sprites are attached in `Update`
by `attach_visuals` to the bodies the tick spawned bare. The join snapshot carries velocity;
one that carried only positions handed the joiner a body at rest where the host's was moving.
`ChecksumLogPlugin<GameHash>` (in `PhysicsSystems::Last`) and `ChecksumExchangePlugin` are
wired in, and a desync is an `error!` and a line in the UI. A departed player's body stays:
there is no tick every peer agrees the player left on until part 2 puts a leave on the tick
timeline, and despawning on the frame the roster changed would desync exactly as the old
spawn did.

### Tests

`crates/bevy_ticked_lockstep_networking/tests/{join_buffers,checksum_lifecycle,trust}.rs`, over
a shared `tests/common` fixture whose world is one counter. `testing::participant_joined_at` is
new; `testing::push_action` panics when the peer has no `LocalPendingActions<A>` (an integer
literal defaulting to `i32` on a `u8` peer used to push nothing and pass).

## T5 — `TickHolds`: one pause vocabulary

Nothing on the wire changed.

### `TicksPaused` is gone; `TickHolds` replaces it

**Before** one marker resource. A joining client inserted it until its first snapshot, a
lockstep peer inserted and removed it every frame from what it had received, a game inserted
it for the pause menu, and every `remove_resource::<TicksPaused>()` lifted everybody's. A
user's pause was undone by the first snapshot to arrive; a lockstep client's wait was undone
by the game's unpause.

**After** `TickHolds`, a set of `TickHoldReason`s: `Manual`, `AwaitingSync`, `SessionPause`,
`WaitingForPeers`, `SoftHold`, `Custom(u8)`. The clock advances when the set is empty. Each
subsystem holds and releases its own reason and never touches another's: `reset_on_join`
holds `AwaitingSync` and the first snapshot releases it; the lockstep sync sets
`WaitingForPeers`; `reset_on_leave` and `reset_on_host` release the session's reasons only.

```rust
// Before
commands.insert_resource(TicksPaused);         // pause
commands.remove_resource::<TicksPaused>();     // resume
if ticks_paused.is_some() { .. }               // read
// After
holds.hold(TickHoldReason::Manual);
holds.release(TickHoldReason::Manual);
if holds.is_held() { .. }                      // any reason
if holds.holds(TickHoldReason::Manual) { .. }  // this one
```

**Delete** a game's `toggle_ticks_paused` that re-inserted the marker every frame to fight
the stack, and any `remove_resource::<TicksPaused>()` on lobby join. **Watch** a UI that
showed "PAUSED" from the marker: `is_held()` says the clock is stopped, `reasons()` says why,
which is the difference between "paused" and "waiting for the host".

### A paused host keeps broadcasting

While its clock is held, a host still sends its snapshot, once every 32 passes of the loop
instead of every tick, so a client that joins during a pause receives the world. A client
receiving the same tick repeatedly drops the duplicates as stale.

`SnapshotApplied.first` now means "the client was holding `AwaitingSync`", which is what it
meant before under another name.

## T4 — core correctness, no wire change

Nothing on the wire changed. Every peer should still be rebuilt: the client's rollback now
restores more, and a client from before this phase disagrees with one from after about what a
correction leaves behind.

### The blend never reaches the simulation (`TickedSystems::Restore`)

`TickedInterpolationPlugin` writes a blended `Transform` for the renderer. It used to stay in
the component into the next tick, which then integrated from a transform `fraction` of the way
back toward the previous tick: a body meant to cross 99 units in 99 ticks crossed 75, and the
games that measured it read it as "physics feels floaty under Hz" and pinned the tick source
to `FixedUpdate`. `TickedSystems::Restore` is a new first set in `TickedLoop`; the plugin puts
the true transform (and `GlobalTransform`, for entities without a parent) back there before
anything reads it.

**Delete** the game's own `transform_to_position: false` workaround if it was only there for
this, and the `FixedUpdate` pin. **Watch** anything else that writes a presentation value into
a simulated component between ticks: put its undo in `TickedSystems::Restore`.
`TickedInterpolation::current()` is new and returns the simulation's value.

### Networked apps refuse `TickSource::FixedUpdate`

`TickedClientPlugin`, `TickedServerPlugin` and `LockstepPlugin` panic in `finish` when
`TickedPlugin` was built with `FixedUpdate`, with a message that says what to change. A
client steers its prediction lead by running a couple of percent fast or slow, and Bevy's
fixed clock cannot be stretched; on `FixedUpdate` it nudged by a whole tick instead, which is
a visible jump in everything the simulation draws. `TickedPlugin::default()` keeps
`FixedUpdate` for solo play and scrubbing; `TickSource::Manual` is still accepted (a test
harness paces it).

```rust
// Before
app.add_plugins(TickedPlugin::default());
// After
app.add_plugins(TickedPlugin { source: TickSource::Hz(64.0), ..default() })
   .add_plugins(TickedInterpolationPlugin);
```

`ConfiguredTickSource` (resource), `TickSource::is_steerable()` and
`require_steerable_tick_source(app, "MyPlugin")` are new for plugins with the same need.

### A client's correction restores everything, not only what travelled

`handle_server_snapshot` now restores rollback-only components and resources (registered
without a wire name) to their value at the snapshot's tick before replaying, and truncates
the `TickedEvents` logs after it. Before, a rollback-only component kept the client's last
predicted value into the replay, and an event from a tick the authority erased stayed
presented. `TickedComponentRegistry::restore_local_only` is the new entry point.

### The frame clocks read the tick inside the simulation

`run_tick_schedule` swaps `Time<Virtual>`, `Time<Fixed>` and `Time<Real>` to the tick's
delta and elapsed for the duration of the tick, and puts them back. A timer or tween that
read `Time<Virtual>` inside a tick used to get the frame's delta on the first run and the
tick's on a replay. The source guard still flags the read; `Time` is the one to use.

### `TickedSimulation` runs single-threaded

`TickedPlugin::simulation_executor` defaults to `SimulationExecutor::SingleThreaded`. Two
unordered systems that write the same component ran in an order that differed per frame and
per peer. A game with every ordering explicit can set `MultiThreaded` back.

### History is sized by whoever knows

`TickedPlugin::history_ticks: Option<u64>`. `None` (the default) installs the hundred-second
scrubbing window and lets a role plugin replace it: `TickedClientPlugin` installs `128` ticks,
which is every tick a snapshot can still name. `Some(n)` is the game's choice and is never
replaced (`HistoryWindowChosen` marks it). **Delete** a game's `HistoryBufferTicks(256)`
unless it scrubs.

### Manual steps run the whole loop

`StepForward` runs `TickedLoop` with `StepOnce` present, so `PreTick` and `PostTick` run on
a step (the interpolation pair shifts, a host broadcasts, a checksum samples); `StepBackward`
and `ResetToTick` run the loop with `RestoredThisPass` present, so everything sees the
restored world and the tick does not advance. Scrubbing is no longer a different simulation
from playing.

### Examples

The networked examples build with `Hz(64.0)` and integrate from `Time::delta_secs()`;
`SECONDS_PER_TICK` is no longer read inside a tick anywhere in this repository. Do the same:
inside a tick `Time` is the tick clock, on the first run and on every replay.

See also `docs/ROLLBACK_RULES.md`, new, for the rules a simulation has to obey and the clippy
configuration that catches a plain `despawn`.

### The host's input window (F8)

`collect_network_inputs` now refuses an input whose tick is outside
`[server_tick - HistoryBufferTicks, server_tick + MAX_INPUT_LEAD_TICKS]`. A refused input is
counted in `InputStats.dropped_out_of_window` (which was reserved in T3 and is live now) and
then ignored entirely: it does not enter the queue, does not move the sender's margin, and
does not become the sender's newest tick. Before this one client could file a tick at
`u64::MAX`, and every snapshot then carried a nine-quintillion margin that no lead could shed.

`bevy_ticked_networking::input::MAX_INPUT_LEAD_TICKS` (64) is the upper edge. It **must equal**
the client's `ClientTickBuffer::MAX_TICKS`, which is now spelled in terms of it; a game that
raised the client's ceiling by hand must raise this one too, or its clients are refused at the
new ceiling. The lower edge is the prune window, because an older tick is unreplayable anyway.

**New message: `messages::PeerLeft(pub u128)`.** A transport writes it on the host when a
client leaves, and the server forgets that uuid's queued inputs at every tick, its
`InputMargins` entry and its `NewestInputTick` mark. `TickedEnsembleSessionPlugin` writes it
when a `LobbyClient` is removed, so a game on the ensemble bridge does nothing. A game on its
own transport triggers it itself (`commands.trigger(PeerLeft(uuid))`); without it a departed
player's body keeps obeying its last keypress until the window prunes it, and a rejoin under
the same uuid finds all of its inputs older than "newest".

### `InputQueue` is ordered (F31, part)

`InputQueue<T>::inputs` is `BTreeMap<u64, BTreeMap<u128, T>>` (was a `HashMap` of `HashMap`s)
and `at_tick` returns `Option<&BTreeMap<u128, T>>`. A simulation that folded over "every
player's input this tick" saw a different order on every peer, because `RandomState` is seeded
per process, and diverged wherever the order mattered. Code that only calls `get`, `insert`,
`at_tick(..).get(&uuid)` or iterates `for (uuid, input) in inputs` compiles unchanged; code
that named the `HashMap` type must say `BTreeMap`. Two additions: `players()` (every uuid in
the queue, once, ascending) and `remove_player(uuid)`.

### Capture allocates nothing (F9, part)

`WorldActions<T>` keeps its history in a `VecDeque` in tick order and recycles the emptied
per-tick maps through a pool; `capture_component` takes one back out with `take_map()` and
caches its query. After warm-up a capture allocates zero bytes
(`tests/allocation_free_capture.rs` counts them). `WorldActions::history` was `pub(crate)`
and is private now; every reader goes through `at_tick`, `recorded_ticks`, `recorded_range`,
`oldest_recorded_tick`, `newest_recorded_tick`. Nothing on the public surface changed.

### `TickedResource: Default`, and `reset_all` (F35)

`TickedResource` (and so `NetworkedTickedResource`) now requires `Default`. A registered
resource is session state by definition, and session state has to have a value that means
"no session". **Add `#[derive(Default)]`** to every registered resource; a type with no
sensible default was never session state and belongs on a tracked entity.

`TickedResourceRegistry::reset_all(&self, world)` puts every registered resource that is
present back to `R::default()` and clears its history. `reset_on_leave` and `reset_on_join`
call it. **Delete** a game's `reset_round_on_leave`-style system: the `RoundState` that walked
into the next lobby still saying "round 7" is what this closes. A registered resource that is
*absent* stays absent; absence is a state the game chose.

## T3 — the testing crate and the numbers a session shows

Pins `bevy_ensemble` at `3ba5229` (E0 loopback harness, E1 trust and liveness, E2 protocol v2,
E3 ICE restart). Nothing on bevy_ticked's own wire changed, but E2 changed the transport's, so
every peer must be rebuilt.

### `bevy_ticked_testing`, a dev-dependency for every game

New crate at `crates/bevy_ticked_testing`. It is the harness the six games each wrote a copy
of, upstream: in-process peers over `bevy_ensemble_loopback` with lossy links, a view of any
peer's world, input scripts, convergence and determinism assertions, a source guard, a wire
guard, goldens, and fault injection. Add it under `[dev-dependencies]` and delete the local
copy; the per-game checklists under `docs/migration/` name which files.

```toml
[dev-dependencies]
bevy_ticked_testing = { git = "https://github.com/Sigma-studios/bevy_ticked" }
# features: "lockstep" for the lockstep peers, "avian" for the avian fixture
```

Its crate docs are the reference. Two things it does that the game copies did not: a peer's
frame rate is a cadence on the network's clock (`set_ticks_per_frame(peer, 2.0)` is a 32 fps
peer that updates every other frame, not a peer whose clock runs twice as fast), and every
assertion has a test in `tests/bites.rs` that shows it failing. The shape of a test:

```rust
use bevy_ticked_testing::prelude::*;

let mut net = TickedNetwork::client_server(host_app, [client_app]);
net.set_link_all(Link::wifi());
net.settle();
net.hold_input(client, Input { forward: true }, 64);
assert_converged::<Pos>(&net, host, client, 8);
```

### Named ensemble registrations

E2 requires every `register_ensemble_message_type` call to carry a wire name. The bridge's own
types are named `bevy_ticked/Snapshot`, `bevy_ticked/Input`, `bevy_ticked/RegistryHandshake`;
lockstep's `bevy_ticked_lockstep/*`. A game registering its own ensemble messages must name
them; see `bevy_ensemble/docs/MIGRATION.md` E2.

### `LOCAL_PLAYER_UUID` is gone

E1 deleted bevy_ensemble's placeholder identity. `bevy_ticked_networking_ensemble` no longer
compares against it; a game that did should delete the comparison. The bridge only adopts a
role once the transport has published a real `LocalMultiplayerPlayerId`.

### `bevy_ticked::checksum` (was `bevy_ticked_lockstep_networking::checksum`)

`WorldHash`, `ChecksumLog`, `ChecksumLogPlugin` and `Divergence` moved into the core crate so a
client-server game can hash its world the same way a lockstep one does. The lockstep crate
re-exports the module, so `bevy_ticked_lockstep_networking::checksum::WorldHash` still resolves;
prefer `bevy_ticked::prelude::*`.

### Diagnostics, always compiled

Every game wrote counters. They are upstream now, counted where the event happens, so a test
and an overlay read the same number:

| Resource | Crate | Inserted by | What it counts |
|---|---|---|---|
| `TickCost` | `bevy_ticked::diagnostics` | `TickedPlugin` | ticks run (replays included), time spent, worst tick |
| `ReplayStats` | `bevy_ticked_networking::diagnostics` | `TickedClientPlugin` | snapshots applied, rollbacks, ticks replayed, last replay distance, stale and pre-role drops |
| `SnapshotStats` | same | `TickedServerPlugin` | broadcasts, bytes (per recipient, filled by the bridge), max and last size |
| `InputStats` | same | `TickedServerPlugin` | inputs received, late |
| `HealthWarnings` | same | `TickedClientPlugin` | client-minted tracked ids, snapshots older than history; each warns once then counts |

**Delete** a game's own `SimRuns`-style counter bracketing `TickedLoop`, its snapshot byte
counter at the bridge, its `debug_assert!` that a client never advanced
`TickTrackedEntityCounter`, and its "measure the frame time" overlay line: `TickCost` measures
the tick, which is the thing this crate runs. Replays count as ticks on purpose.

**Overlay** With the `overlay` feature of `bevy_ticked_networking_ensemble` (default on, pulls
`bevy_ensemble/netdebug`), the bridge publishes `ticked.tick`, `ticked.rate`, `ticked.replay`,
`ticked.snapshot`, `ticked.input` and `ticked.health` lines to `NetDebugExtras`. A game that
published its own under those keys should stop.

**Watch** `ReplayStats.skipped_identical` and `InputStats.dropped_out_of_window` stay zero
until T9 and T4 land; they exist now so a test written today keeps its shape.

### Tests that came upstream with it

`crates/bevy_ticked_networking_ensemble/tests/{lossy_links,alt_tab,determinism}.rs` are
run-2d's network tests over the harness fixture: every link preset, jitter, duplication,
reordering, asymmetric links, three clients, mixed frame rates, a step change in latency, a
ten-thousand-tick session, and both alt-tab cases. `a_host_alt_tab_auto_pauses_and_no_lead_piles_up`
is ignored until the pause phase; today the client runs 132 ticks ahead of a host that
produced none. Every crate has `tests/sim_is_deterministic.rs`, a source guard against
frame-clock and input reads inside the simulation, with dated exceptions.

Two findings the determinism test records rather than hides: the snapshot is a `HashMap` on
the wire, so two encodings of one world can differ in byte order (fixed by the wire phase), and
the client seeds its lead from the ping round trip, which over loopback is the wall clock, so
a trace is reproducible in fates and outcome but not byte for byte.

## T0 — one pin for bevy_ensemble

`bevy_ensemble` and its backends are pinned once, by commit, in the workspace manifest, and
each crate takes them with `workspace = true`. A game pins the same commit its bevy_ticked
revision names, or the two disagree about the transport's wire format. `PLAN.md` is a pointer;
the plan it held described crates that no longer exist.
