//! Which tracked entities existed when, so a rewind can undo a spawn and a despawn.
//!
//! # Why this is not a registered component
//!
//! Rollback restores *state*, and for a long time it could not restore *existence*.
//! `restore_all` walked the entities that currently carried [`TickTrackedEntity`] and, per
//! registered type, inserted the saved value or removed the component. An entity spawned after
//! the tick being restored to appeared in no saved map, so every registered component was
//! stripped from it and nothing despawned it — a **husk**: still tracked, still drawn, carrying
//! none of its state, captured into every future tick for the rest of the session. And an
//! entity despawned after that tick was simply gone: a rope that landed and was cut could not
//! be un-cut by a correction that said it never landed.
//!
//! "Despawn anything with no saved state" is wrong: an entity may legitimately carry none of
//! the registered types at a tick. So existence is recorded separately, per id, as the tick it
//! was born and the tick it died. It costs two `u64`s per tracked entity, ever, and makes the
//! question exact.
//!
//! # Tombstones
//!
//! An entity that should not exist at the tick being restored to is not destroyed: it is
//! *tombstoned* — `Disabled`, so every query and every capture skips it, and unindexed — and
//! kept, because a replay that re-runs the spawn wants the same `Entity` back with the same
//! children, observers and local-only state. [`TrackedSpawner`](crate::tracked_entity::TrackedSpawner)
//! minting the same id revives it. A tombstone the window has passed is reaped.
//!
//! The same shape serves a game's despawn: [`despawn_ticked`](TickedEntityCommandsExt::despawn_ticked)
//! tombstones rather than destroys, so a correction that says the thing is still alive brings
//! it back intact. A plain `despawn` on a tracked entity is caught by an observer that records
//! the death; a rewind past it rebuilds the entity through the spawn path, re-dressed by the
//! game's `On<Add, TickTrackedEntity>` observer, with its local-only state lost. Harmless and
//! loud: it warns once, naming the id.

use std::collections::BTreeMap;

use bevy::ecs::entity_disabling::Disabled;
use bevy::ecs::lifecycle::Despawn;
use bevy::ecs::system::EntityCommands;
use bevy::prelude::*;

use crate::registry::TickedComponentRegistry;
use crate::tick::CurrentTick;
use crate::tracked_entity::TickTrackedEntity;
use crate::tracked_index::TrackedEntityIndex;

/// When a tracked id was alive.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Lifetime {
    /// The first captured tick the id existed at.
    pub born: u64,
    /// The first captured tick it no longer existed at, if it has died.
    pub died: Option<u64>,
}

impl Lifetime {
    pub fn alive_at(&self, tick: u64) -> bool {
        self.born <= tick && self.died.is_none_or(|died| tick < died)
    }
}

/// The lifetime of every tracked id this peer has seen inside the history window.
///
/// Driven by the registry alongside the component histories, so it never describes a tick they
/// no longer cover.
#[derive(Resource, Default)]
pub struct TrackedEntityLifetimes {
    by_id: BTreeMap<u64, Lifetime>,
    /// The newest tick captured, so a death noted between ticks lands on the next one.
    last_captured: Option<u64>,
    /// Reused every capture, so a tick allocates nothing once warm.
    scratch: Vec<u64>,
    query: Option<bevy::ecs::query::QueryState<&'static TickTrackedEntity>>,
}

impl Clone for TrackedEntityLifetimes {
    fn clone(&self) -> Self {
        Self {
            by_id: self.by_id.clone(),
            last_captured: self.last_captured,
            scratch: Vec::new(),
            query: None,
        }
    }
}

impl std::fmt::Debug for TrackedEntityLifetimes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TrackedEntityLifetimes")
            .field("by_id", &self.by_id)
            .finish()
    }
}

impl TrackedEntityLifetimes {
    pub fn lifetime(&self, id: u64) -> Option<Lifetime> {
        self.by_id.get(&id).copied()
    }

    /// Whether `id` existed at `tick`; `None` if the id has never been seen.
    pub fn alive_at(&self, tick: u64, id: u64) -> Option<bool> {
        self.by_id.get(&id).map(|lifetime| lifetime.alive_at(tick))
    }

    pub fn born_at(&self, id: u64) -> Option<u64> {
        self.by_id.get(&id).map(|lifetime| lifetime.born)
    }

    /// Every id alive at `tick`, ascending.
    pub fn alive_ids_at(&self, tick: u64) -> impl Iterator<Item = u64> + '_ {
        self.by_id
            .iter()
            .filter(move |(_, lifetime)| lifetime.alive_at(tick))
            .map(|(id, _)| *id)
    }

    pub fn len(&self) -> usize {
        self.by_id.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_id.is_empty()
    }

    /// Note that `id` exists at `tick`: born now if never seen, revived if it had died.
    pub fn note_alive(&mut self, tick: u64, id: u64) {
        match self.by_id.get_mut(&id) {
            None => {
                self.by_id.insert(
                    id,
                    Lifetime {
                        born: tick,
                        died: None,
                    },
                );
            }
            Some(lifetime) => {
                if lifetime.born > tick {
                    lifetime.born = tick;
                }
                if lifetime.died.is_some_and(|died| died <= tick) {
                    // Alive again after a recorded death: a resurrection the history could
                    // not express with one span. Treat it as born again at `tick`.
                    lifetime.born = tick;
                    lifetime.died = None;
                }
            }
        }
    }

    /// Note that `id` stopped existing at `tick`.
    ///
    /// A death noted after `tick` has been captured (a despawn between ticks) is a death at
    /// the next tick: the world at `tick` had the entity, and a rewind to `tick` must too. A
    /// death noted before the capture (a despawn inside the tick's simulation) is at `tick`.
    pub fn note_dead(&mut self, tick: u64, id: u64) {
        let died = if self.last_captured.is_some_and(|captured| captured >= tick) {
            tick + 1
        } else {
            tick
        };
        if let Some(lifetime) = self.by_id.get_mut(&id)
            && lifetime.died.is_none_or(|d| d > died)
            && lifetime.born <= died
        {
            lifetime.died = Some(died);
        }
    }

    /// The authority's word: `id` did not exist at `tick`, whatever this peer captured.
    pub fn set_died(&mut self, tick: u64, id: u64) {
        if let Some(lifetime) = self.by_id.get_mut(&id)
            && lifetime.born < tick
        {
            lifetime.died = Some(tick);
        } else if let Some(lifetime) = self.by_id.get_mut(&id) {
            // Born at or after the tick it did not exist at: it never existed.
            let _ = lifetime;
            self.by_id.remove(&id);
        }
    }

    /// Record which ids are alive at `tick` from the live world: everything tracked and not
    /// tombstoned is alive; everything known and absent has died.
    pub(crate) fn capture(world: &mut World, tick: u64) {
        let (query, mut scratch) = {
            let mut this = world.get_resource_or_insert_with(Self::default);
            (this.query.take(), std::mem::take(&mut this.scratch))
        };
        let mut query = query.unwrap_or_else(|| world.query::<&TickTrackedEntity>());
        query.update_archetypes(world);
        scratch.clear();
        scratch.extend(query.iter_manual(world).map(|tracked| tracked.0));
        scratch.sort_unstable();

        let mut this = world.resource_mut::<Self>();
        this.last_captured = Some(this.last_captured.map_or(tick, |t| t.max(tick)));
        for id in &scratch {
            this.note_alive(tick, *id);
        }
        for (id, lifetime) in &mut this.by_id {
            if lifetime.died.is_none()
                && lifetime.born <= tick
                && scratch.binary_search(id).is_err()
            {
                lifetime.died = Some(tick);
            }
        }
        this.scratch = scratch;
        this.query = Some(query);
    }

    /// Forget everything after `tick`: an id born after it never existed, a death after it
    /// has not happened.
    pub(crate) fn truncate_after(&mut self, tick: u64) {
        self.last_captured = self.last_captured.map(|t| t.min(tick));
        self.by_id.retain(|_, lifetime| lifetime.born <= tick);
        for lifetime in self.by_id.values_mut() {
            if lifetime.died.is_some_and(|died| died > tick) {
                lifetime.died = None;
            }
        }
    }

    /// Forget ids that died before `tick`.
    pub(crate) fn prune_before(&mut self, tick: u64) {
        self.by_id
            .retain(|_, lifetime| lifetime.died.is_none_or(|died| died >= tick));
    }

    pub(crate) fn clear(&mut self) {
        self.by_id.clear();
        self.last_captured = None;
    }
}

/// A tracked entity that does not exist right now, kept for the rewind or replay that may want
/// it back. `Disabled` travels with it (and its children), so nothing sees it.
#[derive(Component, Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tombstone {
    /// The tick it stopped existing at.
    pub died_at: u64,
}

/// `despawn_ticked`: the despawn a rollback can undo.
pub trait TickedEntityCommandsExt {
    /// Tombstone this tracked entity at the current tick instead of destroying it.
    ///
    /// The entity and its children are `Disabled`: every query, capture and snapshot skips
    /// them, and nothing else changes. A rewind to a tick it was alive at revives it intact —
    /// components, children, observers, local-only state — and a spawn that mints its id
    /// again during a replay reuses it. It is destroyed for real once the history window has
    /// passed its death.
    ///
    /// On an entity that is not tracked this is a plain despawn.
    fn despawn_ticked(&mut self);
}

impl TickedEntityCommandsExt for EntityCommands<'_> {
    fn despawn_ticked(&mut self) {
        self.queue(|mut entity: EntityWorldMut| entity.despawn_ticked());
    }
}

impl TickedEntityCommandsExt for EntityWorldMut<'_> {
    fn despawn_ticked(&mut self) {
        let entity = self.id();
        let Some(id) = self.get::<TickTrackedEntity>().map(|tracked| tracked.0) else {
            self.world_scope(|world| {
                world.despawn(entity);
            });
            return;
        };
        let tick = self
            .world()
            .get_resource::<CurrentTick>()
            .map_or(0, |t| t.0);
        self.world_scope(|world| tombstone(world, entity, id, tick));
    }
}

/// Tombstone `entity` (tracked as `id`) at `tick`: disabled recursively, unindexed, and noted
/// dead in the lifetimes (on the next tick if `tick` was already captured with it alive).
pub fn tombstone(world: &mut World, entity: Entity, id: u64, tick: u64) {
    tombstone_inner(world, entity, id, tick, false);
}

/// As [`tombstone`], with the death at exactly `tick`: the authority said so.
pub fn tombstone_at(world: &mut World, entity: Entity, id: u64, tick: u64) {
    tombstone_inner(world, entity, id, tick, true);
}

fn tombstone_inner(world: &mut World, entity: Entity, id: u64, tick: u64, exact: bool) {
    let Ok(mut entity_mut) = world.get_entity_mut(entity) else {
        return;
    };
    if entity_mut.contains::<Tombstone>() {
        return;
    }
    entity_mut.insert(Tombstone { died_at: tick });
    entity_mut.insert_recursive::<Children>(Disabled);
    if let Some(mut index) = world.get_resource_mut::<TrackedEntityIndex>() {
        index.tombstone(id, entity);
    }
    if let Some(mut lifetimes) = world.get_resource_mut::<TrackedEntityLifetimes>() {
        if exact {
            lifetimes.set_died(tick, id);
        } else {
            lifetimes.note_dead(tick, id);
        }
        if let Some(died) = lifetimes.lifetime(id).and_then(|l| l.died)
            && let Ok(mut entity_mut) = world.get_entity_mut(entity)
        {
            entity_mut.insert(Tombstone { died_at: died });
        }
    }
}

/// Bring a tombstone back: enabled recursively, re-indexed, noted alive.
pub fn revive(world: &mut World, entity: Entity, id: u64, tick: u64) {
    let Ok(mut entity_mut) = world.get_entity_mut(entity) else {
        return;
    };
    entity_mut.remove::<Tombstone>();
    entity_mut.remove_recursive::<Children, Disabled>();
    if let Some(mut index) = world.get_resource_mut::<TrackedEntityIndex>() {
        index.revive(id, entity);
    }
    if let Some(mut lifetimes) = world.get_resource_mut::<TrackedEntityLifetimes>() {
        lifetimes.note_alive(tick, id);
    }
}

/// Every tombstone that died before `tick`: the window has passed it and nothing will ask for
/// it back.
pub(crate) fn reap_before(world: &mut World, tick: u64) {
    // The index knows every tombstone; no query, and nothing to do on the common tick.
    let Some(index) = world.get_resource::<TrackedEntityIndex>() else {
        return;
    };
    if index.tombstones().next().is_none() {
        return;
    }
    let candidates: Vec<Entity> = index.tombstones().map(|(entity, _)| entity).collect();
    let doomed: Vec<Entity> = candidates
        .into_iter()
        .filter(|entity| {
            world
                .get::<Tombstone>(*entity)
                .is_some_and(|tombstone| tombstone.died_at < tick)
        })
        .collect();
    for entity in doomed {
        world.despawn(entity);
    }
}

/// Destroy every tombstone: a session reset.
pub(crate) fn reap_all(world: &mut World) {
    reap_before(world, u64::MAX);
}

/// A plain `despawn` on a tracked entity: record the death so a rewind can rebuild it, and say
/// so once.
pub(crate) fn record_plain_despawn(
    despawn: On<Despawn, TickTrackedEntity>,
    tracked: Query<(&TickTrackedEntity, Has<Tombstone>)>,
    tick: Res<CurrentTick>,
    mut lifetimes: ResMut<TrackedEntityLifetimes>,
) {
    let Ok((id, tombstoned)) = tracked.get(despawn.entity) else {
        return;
    };
    if tombstoned {
        // A reaped tombstone: its death was recorded when it was tombstoned.
        return;
    }
    lifetimes.note_dead(tick.0, id.0);
    warn_once!(
        "tracked entity {} was despawned with a plain `despawn` at tick {}; a rollback past \
         this tick will rebuild it through the snapshot spawn path with its local-only state \
         lost. Use `despawn_ticked` to keep it intact (said once; the game's clippy \
         `disallowed-methods` in docs/ROLLBACK_RULES.md catches the rest)",
        id.0,
        tick.0
    );
}

/// Existence at `tick`, applied to the live world: tombstone what was not alive, revive what
/// was, and rebuild what is missing altogether from the component histories.
///
/// Called by [`TickedComponentRegistry::restore_all`] before the component restore, so every
/// entity that should exist at `tick` does when the components are put back.
pub fn restore_existence(world: &mut World, registry: &TickedComponentRegistry, tick: u64) {
    let Some(lifetimes) = world.get_resource::<TrackedEntityLifetimes>().cloned() else {
        return;
    };
    let live: Vec<(Entity, u64)> = {
        let mut tracked = world.query::<(Entity, &TickTrackedEntity)>();
        tracked.iter(world).map(|(e, t)| (e, t.0)).collect()
    };
    let tombstones: Vec<(Entity, u64)> = world
        .resource::<TrackedEntityIndex>()
        .tombstones()
        .collect();

    // Not alive at `tick`: an entity spawned after it, or one the lifetimes say died before.
    for (entity, id) in &live {
        if lifetimes.alive_at(tick, *id) == Some(false) {
            tombstone(world, *entity, *id, tick);
        }
    }
    // Alive at `tick` but tombstoned now: a despawn the rewind undoes.
    let mut present: std::collections::BTreeSet<u64> = live
        .iter()
        .filter(|(_, id)| lifetimes.alive_at(tick, *id) != Some(false))
        .map(|(_, id)| *id)
        .collect();
    for (entity, id) in &tombstones {
        if lifetimes.alive_at(tick, *id) == Some(true) {
            revive(world, *entity, *id, tick);
            present.insert(*id);
        }
    }
    // Alive at `tick` and nowhere in the world: destroyed by a plain despawn. Rebuild from
    // the histories through the spawn path, `TickTrackedEntity` last so the game's observer
    // dresses it.
    let missing: Vec<u64> = lifetimes
        .alive_ids_at(tick)
        .filter(|id| !present.contains(id))
        .collect();
    for id in missing {
        let entity = world.spawn_empty().id();
        registry.restore_one_from_history(world, tick, id, entity);
        world.entity_mut(entity).insert(TickTrackedEntity(id));
    }
}
