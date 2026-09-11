use std::collections::{HashMap, VecDeque};

use bevy::ecs::query::QueryState;
use bevy::prelude::*;

use crate::{registry::TickedComponent, tracked_entity::TickTrackedEntity};

/// How many emptied per-tick maps are kept for reuse.
///
/// Steady state needs one: each tick takes one map for the capture and the prune hands one
/// back. A rollback hands back its whole replay distance at once and takes them all again
/// during the replay, so the pool is sized for the deepest replay a client makes rather than
/// for one tick. Beyond this they are freed, so a one-off deep rewind does not pin memory for
/// the rest of the session.
const POOL_CAPACITY: usize = 128;

/// Stores the history of a registered component type across all ticks and entities.
///
/// One `WorldActions<T>` resource exists per registered component type.
/// Maps `tick -> (entity_network_id -> component_value)`.
///
/// # Why a deque and a pool, not a map of maps
///
/// This used to be a `BTreeMap<u64, HashMap<u64, T>>`, with a fresh `HashMap` built for every
/// capture and dropped by every prune. Per registered type, per tick, that was one map
/// allocation, a rehash as it grew past each power of two, and — because a `BTreeMap` splits and
/// merges nodes as keys march through it — a node allocation every few ticks on top. Sixty-four
/// times a second, times the number of registered types, on every peer, for the life of the
/// session: the capture path was the steadiest source of allocator traffic in a game that
/// otherwise had none inside its tick, and it showed up as the periodic hitch a profiler blamed
/// on the allocator rather than on anything the game did.
///
/// History is contiguous and pushed at one end and popped at the other, which is a ring, so
/// `history` is a `VecDeque` in tick order: after warm-up neither end moves the buffer. The maps
/// are recycled through `pool` instead of freed, and [`take_map`](Self::take_map) hands one back
/// out — emptied, but with its buckets intact — so a capture of the same entities as last tick
/// inserts into capacity it already has. The query state is kept too, because building one is
/// an allocation as well. After warm-up a capture allocates nothing at all; the test
/// `capturing_a_tick_after_warmup_allocates_nothing` holds it to that.
#[derive(Resource)]
pub struct WorldActions<T: TickedComponent> {
    /// `(tick, state)` in ascending tick order. Ticks need not be contiguous.
    history: VecDeque<(u64, HashMap<u64, T>)>,
    /// Emptied maps waiting to be filled by the next capture.
    pool: Vec<HashMap<u64, T>>,
    /// The capture query, built once and cached. `None` only until the first capture, and
    /// while a capture has it checked out.
    query: Option<QueryState<(&'static TickTrackedEntity, &'static T)>>,
}

impl<T: TickedComponent> Default for WorldActions<T> {
    fn default() -> Self {
        Self {
            history: VecDeque::new(),
            pool: Vec::new(),
            query: None,
        }
    }
}

impl<T: TickedComponent> WorldActions<T> {
    /// Position of `tick` in `history`, or where it would be inserted.
    fn position(&self, tick: u64) -> Result<usize, usize> {
        self.history.binary_search_by_key(&tick, |(t, _)| *t)
    }

    /// Get the state of all entities at a given tick.
    pub fn at_tick(&self, tick: u64) -> Option<&HashMap<u64, T>> {
        let index = self.position(tick).ok()?;
        self.history.get(index).map(|(_, state)| state)
    }

    /// The oldest tick still retained in history, if any.
    pub fn oldest_recorded_tick(&self) -> Option<u64> {
        self.history.front().map(|(tick, _)| *tick)
    }

    /// The newest tick recorded in history, if any.
    pub fn newest_recorded_tick(&self) -> Option<u64> {
        self.history.back().map(|(tick, _)| *tick)
    }

    /// The inclusive `(oldest, newest)` range of recorded ticks, if any exist.
    ///
    /// Useful for sizing a scrub bar precisely instead of guessing from
    /// `CurrentTick - HISTORY_BUFFER_TICKS`.
    pub fn recorded_range(&self) -> Option<(u64, u64)> {
        Some((self.oldest_recorded_tick()?, self.newest_recorded_tick()?))
    }

    /// Iterate over all recorded ticks in ascending order.
    pub fn recorded_ticks(&self) -> impl DoubleEndedIterator<Item = u64> + '_ {
        self.history.iter().map(|(tick, _)| *tick)
    }

    /// Insert state for a specific entity at a specific tick.
    pub fn insert(&mut self, tick: u64, entity_network_id: u64, component: T) {
        let index = match self.position(tick) {
            Ok(index) => index,
            Err(index) => {
                let state = self.take_map();
                self.history.insert(index, (tick, state));
                index
            }
        };
        self.history[index].1.insert(entity_network_id, component);
    }

    /// An empty map to fill for a capture, from the pool when it has one.
    ///
    /// Hand it back through [`set_tick`](Self::set_tick); a map taken and dropped is merely an
    /// allocation, not an error.
    pub fn take_map(&mut self) -> HashMap<u64, T> {
        self.pool.pop().unwrap_or_default()
    }

    /// Replace all state at a given tick.
    pub fn set_tick(&mut self, tick: u64, state: HashMap<u64, T>) {
        match self.position(tick) {
            Ok(index) => {
                let old = std::mem::replace(&mut self.history[index].1, state);
                self.recycle(old);
            }
            Err(index) => self.history.insert(index, (tick, state)),
        }
    }

    /// Remove all history after a given tick (exclusive).
    /// Used after rollback to discard invalidated future state.
    pub fn truncate_after(&mut self, tick: u64) {
        while self.history.back().is_some_and(|(t, _)| *t > tick) {
            let (_, state) = self.history.pop_back().expect("checked non-empty");
            self.recycle(state);
        }
    }

    /// Remove all history before a given tick.
    /// Used to bound memory growth during long sessions.
    pub fn prune_before(&mut self, tick: u64) {
        while self.history.front().is_some_and(|(t, _)| *t < tick) {
            let (_, state) = self.history.pop_front().expect("checked non-empty");
            self.recycle(state);
        }
    }

    /// Clear all history.
    pub fn clear(&mut self) {
        while let Some((_, state)) = self.history.pop_back() {
            self.recycle(state);
        }
    }

    /// Empty a map and keep it for the next capture, unless the pool is full.
    fn recycle(&mut self, mut state: HashMap<u64, T>) {
        if self.pool.len() < POOL_CAPACITY {
            state.clear();
            self.pool.push(state);
        }
    }

    /// Check the cached capture query out. `None` until the first capture has built one.
    ///
    /// Checked out rather than borrowed because iterating it needs `&World` while it lives in
    /// a resource of that same world. Return it with [`put_query`](Self::put_query).
    pub(crate) fn take_query(
        &mut self,
    ) -> Option<QueryState<(&'static TickTrackedEntity, &'static T)>> {
        self.query.take()
    }

    /// Return the capture query taken by [`take_query`](Self::take_query).
    pub(crate) fn put_query(
        &mut self,
        query: QueryState<(&'static TickTrackedEntity, &'static T)>,
    ) {
        self.query = Some(query);
    }
}
