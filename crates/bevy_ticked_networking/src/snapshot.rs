use std::collections::{HashMap, HashSet};

use bevy::prelude::*;
use serde::{Deserialize, Serialize};

use bevy_ticked::{
    lifetimes::TrackedEntityLifetimes,
    registry::TickedComponentRegistry,
    tick::CurrentTick,
    tracked_entity::{TickTrackedEntity, TickTrackedEntityCounter},
};

/// A serializable snapshot of the tracked world state at a specific tick.
///
/// # Existence is a field, not an inference
///
/// [`entities`](Self::entities) lists every tracked entity alive at [`tick`](Self::tick), and it
/// is the authority on what exists. Existence used to be inferred from the union of the component
/// maps, which had two consequences worth knowing about because code was written around both: an
/// entity stripped of all its networked components vanished from every peer while still alive on
/// the host, and — the reason this field had to exist — a snapshot carrying only what *changed*
/// would read as "everything else was despawned".
///
/// # A delta is not a smaller full snapshot
///
/// When [`keyframe`](Self::keyframe) is true, [`components`](Self::components) is the complete
/// state and absence within a present type means the authority does not have that component.
/// When it is false, `components` holds only what changed since this stream's last send, absence
/// means *unchanged*, and removals travel explicitly in [`removed`](Self::removed). The two
/// meanings are opposite, which is why the flag is on the wire rather than inferred from a size.
///
/// A receiver must have absorbed a keyframe before a delta means anything —
/// [`SnapshotBaseline::primed`] is that question.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorldSnapshot {
    pub tick: u64,
    /// Every tracked entity alive at `tick`, ascending. Authoritative for existence.
    ///
    /// Sorted rather than in hash order so that two runs of the same simulation produce the same
    /// bytes — which is what makes a snapshot's size a measurement rather than a sample.
    #[serde(default)]
    pub entities: Vec<u64>,
    /// component_type_index -> (tracked_entity_id -> serialized_component_bytes)
    pub components: HashMap<u16, HashMap<u64, Vec<u8>>>,
    /// `(type index, entity id)` pairs the authority no longer has a component for.
    ///
    /// Only meaningful on a delta. On a keyframe removal is carried by absence, and this is empty.
    #[serde(default)]
    pub removed: Vec<(u16, u64)>,
    /// Whether `components` is the whole state (`true`) or only what changed (`false`).
    pub keyframe: bool,
    /// Per-client input-arrival margin in ticks, measured by the server: how many
    /// ticks *ahead* of the server that client's most recent input arrived
    /// (negative = arrived late). Clients read their own entry to size their
    /// prediction lead. Populated by the server; empty in `build_snapshot`.
    #[serde(default)]
    pub input_margins: HashMap<u128, i64>,
}

/// Build a complete snapshot of the world state at the given tick.
///
/// Always a keyframe. `input_margins` is left empty here; the server fills it in before
/// broadcasting, and [`SnapshotBaseline::reduce`] is what turns this into a delta if the server
/// is configured to send them.
pub fn build_snapshot(world: &mut World, tick: u64) -> WorldSnapshot {
    let registry = world.resource::<TickedComponentRegistry>().clone();
    let components = registry.serialize_all(world, tick);
    let entities = entities_at(world, tick, &components);
    WorldSnapshot {
        tick,
        entities,
        components,
        removed: Vec::new(),
        keyframe: true,
        input_margins: HashMap::new(),
    }
}

/// Which tracked entities existed at `tick`.
///
/// Prefers the lifetime history, which records existence independently of any component and so
/// can say that an entity carrying nothing registered was nonetheless there. Falls back to the
/// union of the component maps — what existence used to mean — for a world that never captured a
/// lifetime record, which is any caller building a snapshot for a tick it did not capture.
fn entities_at(
    world: &World,
    tick: u64,
    components: &HashMap<u16, HashMap<u64, Vec<u8>>>,
) -> Vec<u64> {
    if let Some(alive) = world
        .get_resource::<TrackedEntityLifetimes>()
        .and_then(|lifetimes| lifetimes.at_tick(tick))
    {
        let mut ids: Vec<u64> = alive.iter().copied().collect();
        ids.sort_unstable();
        return ids;
    }
    let mut ids: Vec<u64> = components
        .values()
        .flat_map(|entities| entities.keys().copied())
        .collect();
    ids.sort_unstable();
    ids.dedup();
    ids
}

/// What has been sent, or received, for each `(type index, entity id)`.
///
/// One of these sits on the server — the state it believes every client has — and one on each
/// client. They are the same structure read from opposite ends, and they are only ever in step
/// because every client sees every snapshot. **Snapshots are unreliable**, so a client that drops
/// one is wrong about everything that changed in it until the next keyframe. That is the trade a
/// delta buys its bandwidth with, and it is why `keyframe_every` defaults to 1 (never delta).
///
/// A proper fix is per-recipient baselines against an acknowledged sequence number, which needs a
/// sequence number on the payload first.
#[derive(Resource, Default, Clone)]
pub struct SnapshotBaseline {
    entries: HashMap<u16, HashMap<u64, Vec<u8>>>,
    primed: bool,
}

impl SnapshotBaseline {
    /// Whether a keyframe has been absorbed, and therefore whether a delta can be applied at all.
    pub fn primed(&self) -> bool {
        self.primed
    }

    /// Forget everything. Used when a session ends, so the next one does not decode its deltas
    /// against the last one's state.
    pub fn reset(&mut self) {
        self.entries.clear();
        self.primed = false;
    }

    /// Reduce a complete snapshot to what this stream has not already sent.
    ///
    /// Consumes nothing and mutates the baseline to match what it returns, so calling it is a
    /// commitment to send the result. `rates` may hold a type back for a few ticks; a held type is
    /// simply not compared this tick, and its change goes out on the next tick it is due.
    pub fn reduce(&mut self, full: &WorldSnapshot, rates: &SnapshotSendRates) -> WorldSnapshot {
        // Existence carries entity death, so drop what the baseline knows about the departed
        // rather than emitting a removal per component per dead entity.
        let alive: HashSet<u64> = full.entities.iter().copied().collect();
        for known in self.entries.values_mut() {
            known.retain(|id, _| alive.contains(id));
        }

        let mut components: HashMap<u16, HashMap<u64, Vec<u8>>> = HashMap::new();
        let mut removed: Vec<(u16, u64)> = Vec::new();

        for (type_index, entities) in &full.components {
            if !rates.due(*type_index, full.tick) {
                continue;
            }
            let known = self.entries.entry(*type_index).or_default();
            let mut changed: HashMap<u64, Vec<u8>> = HashMap::new();
            for (id, bytes) in entities {
                if known.get(id) != Some(bytes) {
                    changed.insert(*id, bytes.clone());
                }
            }
            for id in known.keys() {
                if !entities.contains_key(id) {
                    removed.push((*type_index, *id));
                }
            }
            if !changed.is_empty() {
                components.insert(*type_index, changed);
            }
        }

        for (type_index, changed) in &components {
            let known = self.entries.entry(*type_index).or_default();
            for (id, bytes) in changed {
                known.insert(*id, bytes.clone());
            }
        }
        for (type_index, id) in &removed {
            if let Some(known) = self.entries.get_mut(type_index) {
                known.remove(id);
            }
        }
        removed.sort_unstable();

        WorldSnapshot {
            tick: full.tick,
            entities: full.entities.clone(),
            components,
            removed,
            keyframe: false,
            input_margins: full.input_margins.clone(),
        }
    }

    /// Record a keyframe as the new baseline, and return it unchanged.
    pub fn prime(&mut self, full: &WorldSnapshot) {
        self.entries = full.components.clone();
        self.primed = true;
    }

    /// Fold a snapshot into the baseline and return the complete state it now describes.
    ///
    /// A keyframe replaces the baseline; a delta is layered onto it. Either way what comes back is
    /// the full component map for `snapshot.tick`, which is what `apply_snapshot` wants — the
    /// saving is on the wire, not in the applying.
    pub fn absorb(&mut self, snapshot: &WorldSnapshot) -> HashMap<u16, HashMap<u64, Vec<u8>>> {
        if snapshot.keyframe {
            self.entries = snapshot.components.clone();
            self.primed = true;
        } else {
            for (type_index, changed) in &snapshot.components {
                let known = self.entries.entry(*type_index).or_default();
                for (id, bytes) in changed {
                    known.insert(*id, bytes.clone());
                }
            }
            for (type_index, id) in &snapshot.removed {
                if let Some(known) = self.entries.get_mut(type_index) {
                    known.remove(id);
                }
            }
        }
        let alive: HashSet<u64> = snapshot.entities.iter().copied().collect();
        if !alive.is_empty() {
            for known in self.entries.values_mut() {
                known.retain(|id, _| alive.contains(id));
            }
        }
        self.entries.clone()
    }
}

/// How often each component type is allowed onto the wire, by registry index.
///
/// A type set to *N* is compared and sent on ticks where `tick % N == 0` and on every keyframe.
/// Between those it simply is not looked at, so a change is delayed by at most `N - 1` ticks.
/// Intended for state that changes rarely or that nobody can see change quickly — a score, a
/// round number, a colour — and wrong for anything a player's own input drives.
#[derive(Resource, Default)]
pub struct SnapshotSendRates {
    rates: HashMap<u16, u64>,
}

impl SnapshotSendRates {
    pub fn set(&mut self, type_index: u16, every: u64) {
        self.rates.insert(type_index, every.max(1));
    }

    pub fn get(&self, type_index: u16) -> u64 {
        self.rates.get(&type_index).copied().unwrap_or(1)
    }

    fn due(&self, type_index: u16, tick: u64) -> bool {
        match self.rates.get(&type_index) {
            Some(&every) if every > 1 => tick % every == 0,
            _ => true,
        }
    }
}

/// Apply a network snapshot: sync entity lifecycle, apply component state, update tick.
///
/// `components` must be the **complete** state for the tick — on a delta stream that is what
/// [`SnapshotBaseline::absorb`] returns, not the snapshot's own field.
///
/// - Entities in `snapshot.entities` but not local are **spawned**
/// - Local tracked entities not in `snapshot.entities` are **despawned**
/// - Existing entities get their components updated
///
/// Newly spawned entities get all networked components inserted first, then
/// `TickTrackedEntity` is inserted last. This means `On<Add, TickTrackedEntity>`
/// observers can read the networked components via Query.
pub fn apply_snapshot(world: &mut World, snapshot: &WorldSnapshot) {
    let components = snapshot.components.clone();
    apply_snapshot_with(world, snapshot, &components);
}

/// [`apply_snapshot`], with the complete component map supplied separately.
pub fn apply_snapshot_with(
    world: &mut World,
    snapshot: &WorldSnapshot,
    components: &HashMap<u16, HashMap<u64, Vec<u8>>>,
) {
    let registry = world.resource::<TickedComponentRegistry>().clone();

    // 1. The set of entity IDs that exist at this tick.
    //
    //    From `entities` when it is populated. The fallback is the union of the component maps,
    //    which is what existence meant before the field was added, and it agrees with `entities`
    //    for every entity carrying at least one networked component.
    let snapshot_entity_ids: HashSet<u64> = if snapshot.entities.is_empty() {
        components
            .values()
            .flat_map(|entities| entities.keys().copied())
            .collect()
    } else {
        snapshot.entities.iter().copied().collect()
    };

    // 2. Query all existing TickTrackedEntity entities
    let mut query = world.query::<(Entity, &TickTrackedEntity)>();
    let existing: Vec<(Entity, u64)> = query
        .iter(world)
        .map(|(e, tte)| (e, tte.0))
        .collect();

    let existing_ids: HashSet<u64> = existing.iter().map(|(_, id)| *id).collect();

    // 3. Despawn local entities NOT in the snapshot.
    //    Uses world.despawn() (immediate) rather than deferred commands so that
    //    the query in deserialize_and_apply_all (step 5) does not see them.
    for (entity, id) in &existing {
        if !snapshot_entity_ids.contains(id) {
            world.despawn(*entity);
        }
    }

    // 4. Spawn entities in the snapshot but NOT local
    for &new_id in &snapshot_entity_ids {
        if existing_ids.contains(&new_id) {
            continue;
        }

        let entity = world.spawn_empty().id();

        // Insert all networked components from the snapshot for this entity
        for (type_index, entities) in components {
            if let Some(bytes) = entities.get(&new_id) {
                registry.deserialize_and_insert_one(world, *type_index, entity, bytes);
            }
        }

        // Insert TickTrackedEntity LAST so On<Add> observers can read components
        world.entity_mut(entity).insert(TickTrackedEntity(new_id));
    }

    // 5. Apply snapshot to existing (surviving) entities + write into WorldActions
    registry.deserialize_and_apply_all(world, snapshot.tick, components);

    // 6. Reset counter to max snapshot ID so that rollback+replay produces
    //    deterministic entity IDs matching the server.
    let max_id = snapshot_entity_ids.iter().max().copied().unwrap_or(0);
    world.resource_mut::<TickTrackedEntityCounter>().0 = max_id;

    // 7. Set current tick
    world.resource_mut::<CurrentTick>().0 = snapshot.tick;
}
