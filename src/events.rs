//! Saying "this happened at tick *N*", exactly once, even under rollback.
//!
//! # The problem this exists for
//!
//! [`TickedSimulation`](crate::TickedSimulation) is re-run once per replayed tick.
//! A client that receives a snapshot for a tick it has already predicted past runs
//! the whole simulation again for every tick in between — several times a frame,
//! for ever. So a `Message` written from a simulation system is written several
//! times per frame, and the obvious design for a game with sound — gameplay
//! triggers `PropDestroyed`, an audio observer plays a thunk — stutters and
//! double-fires the moment anybody's connection wobbles.
//!
//! Every consumer of this crate hit that and worked around it differently:
//! spawning a tracked entity per explosion purely so that `On<Add>` would fire once
//! (which loses the event outright if the snapshot carrying it is dropped);
//! diffing a deliberately unregistered mirror of the world in `Update`; draining a
//! plain `Vec` that rollback does not know about. All three are the same missing
//! feature seen from different angles.
//!
//! # What this does instead
//!
//! [`TickedEvents<T>`] is a log keyed by tick, not a queue. Writing is idempotent
//! per tick: a replay of tick *N* **replaces** what tick *N* said rather than
//! appending to it, so running the same tick five times leaves the same one entry.
//! Reading is watermarked: [`TickedEventReader`] hands out each tick's events once
//! and remembers how far it has presented.
//!
//! That combination is what makes rollback survivable. A tick that is replayed and
//! comes out the same was already presented and stays presented. A tick that is
//! replayed and comes out *different* — because a snapshot corrected it — rewinds
//! the watermark, so the corrected version is presented and the prediction that
//! never really happened is not.
//!
//! # What it is not
//!
//! Not a transport. These do not travel; each peer writes its own from its own
//! simulation. That is the point — an event derived identically on every peer costs
//! nothing to send, and one that is *not* derivable is a fact about the world and
//! belongs in a replicated component.
//!
//! # Using it
//!
//! ```rust,ignore
//! app.add_ticked_event::<PropDestroyed>();
//!
//! // In TickedSimulation: state the fact. Replay-safe, no guard needed.
//! fn fell_prop(tick: Res<CurrentTick>, mut events: TickedEventWriter<PropDestroyed>) {
//!     events.write(tick.0, PropDestroyed { prop: 7 });
//! }
//!
//! // In Update: present it, exactly once.
//! fn play_thunks(mut events: TickedEventReader<PropDestroyed>, mut audio: Audio) {
//!     for (tick, event) in events.read() {
//!         audio.play_at(event.prop, tick);
//!     }
//! }
//! ```

use std::collections::BTreeMap;

use bevy::ecs::system::SystemParam;
use bevy::prelude::*;

use crate::tick::{CurrentTick, HistoryBufferTicks};

/// Trait bound for anything that can be logged per tick.
pub trait TickedEvent: Send + Sync + Clone + 'static {}
impl<T> TickedEvent for T where T: Send + Sync + Clone + 'static {}

/// A per-tick log of `T`, rollback-aware and presented exactly once.
///
/// See the [module docs](self) for why this is not a `Message`.
#[derive(Resource)]
pub struct TickedEvents<T: TickedEvent> {
    /// tick -> everything that happened at that tick, in the order it was written.
    log: BTreeMap<u64, Vec<T>>,
    /// The highest tick already handed to a reader. Rewound when a replayed tick
    /// disagrees with what it said the first time.
    presented: u64,
    /// The tick currently being written, so the first write of a replay clears it.
    writing: Option<u64>,
}

impl<T: TickedEvent> Default for TickedEvents<T> {
    fn default() -> Self {
        Self {
            log: BTreeMap::new(),
            presented: 0,
            writing: None,
        }
    }
}

impl<T: TickedEvent> TickedEvents<T> {
    /// Record that `event` happened at `tick`.
    ///
    /// The first write for a given tick discards anything that tick said
    /// previously, which is what makes a replay idempotent rather than cumulative.
    /// A tick that produced two events the first time and two events the second
    /// time holds two, not four.
    pub fn write(&mut self, tick: u64, event: T) {
        if self.writing != Some(tick) {
            self.writing = Some(tick);
            self.log.insert(tick, Vec::new());
        }
        self.log.entry(tick).or_default().push(event);
    }

    /// Everything recorded at `tick`.
    pub fn at_tick(&self, tick: u64) -> &[T] {
        self.log.get(&tick).map(Vec::as_slice).unwrap_or(&[])
    }

    /// Discard everything after `tick`, and rewind the presentation watermark to
    /// match.
    ///
    /// Called when a rollback invalidates predicted ticks. Rewinding `presented` is
    /// the half that is easy to forget and impossible to notice: without it, the
    /// corrected version of a tick would be silently swallowed as "already shown".
    pub fn truncate_after(&mut self, tick: u64) {
        self.log.split_off(&(tick + 1));
        self.presented = self.presented.min(tick);
        self.writing = None;
    }

    /// Drop everything before `tick`.
    pub fn prune_before(&mut self, tick: u64) {
        self.log = self.log.split_off(&tick);
        self.presented = self.presented.max(tick.saturating_sub(1));
    }

    /// Clear the whole log, for a session reset.
    pub fn clear(&mut self) {
        self.log.clear();
        self.presented = 0;
        self.writing = None;
    }

    /// Everything not yet presented, oldest first, and advance the watermark.
    fn drain_unpresented(&mut self, up_to: u64) -> Vec<(u64, T)> {
        let mut out = Vec::new();
        // `BTreeMap::range` panics on an inverted range rather than yielding
        // nothing, and "everything is already presented" inverts it — which is the
        // common case, once per frame, on every frame where nothing happened.
        let from = self.presented.saturating_add(1);
        if from <= up_to {
            for (&tick, events) in self.log.range(from..=up_to) {
                out.extend(events.iter().cloned().map(|event| (tick, event)));
            }
        }
        self.presented = self.presented.max(up_to);
        out
    }
}

/// Write ticked events from inside [`TickedSimulation`](crate::TickedSimulation).
#[derive(SystemParam)]
pub struct TickedEventWriter<'w, T: TickedEvent> {
    events: ResMut<'w, TickedEvents<T>>,
}

impl<T: TickedEvent> TickedEventWriter<'_, T> {
    /// Record that `event` happened at `tick`. Safe to call from a replayed tick.
    pub fn write(&mut self, tick: u64, event: T) {
        self.events.write(tick, event);
    }
}

/// Read ticked events from `Update`, each exactly once.
#[derive(SystemParam)]
pub struct TickedEventReader<'w, T: TickedEvent> {
    events: ResMut<'w, TickedEvents<T>>,
    tick: Res<'w, CurrentTick>,
}

impl<T: TickedEvent> TickedEventReader<'_, T> {
    /// Everything that has happened since the last read, as `(tick, event)`.
    ///
    /// One reader per event type. Two systems reading the same `T` would race for
    /// the watermark and each see roughly half — read once and fan out from there,
    /// which is what a presentation layer wants anyway.
    pub fn read(&mut self) -> Vec<(u64, T)> {
        let now = self.tick.0;
        self.events.drain_unpresented(now)
    }
}

/// Register a per-tick event type.
pub trait TickedEventAppExt {
    /// Add a [`TickedEvents<T>`] log, pruned on the same window as component
    /// history.
    fn add_ticked_event<T: TickedEvent>(&mut self) -> &mut Self;
}

impl TickedEventAppExt for App {
    fn add_ticked_event<T: TickedEvent>(&mut self) -> &mut Self {
        if self.world().contains_resource::<TickedEvents<T>>() {
            return self;
        }
        self.init_resource::<TickedEvents<T>>()
            .init_resource::<TickedEventRegistry>();
        self.world_mut()
            .resource_mut::<TickedEventRegistry>()
            .0
            .push(truncate_one::<T>);
        self.add_systems(
            crate::TickedLoop,
            prune_ticked_events::<T>.in_set(crate::TickedSystems::PostTick),
        )
    }
}

fn prune_ticked_events<T: TickedEvent>(
    tick: Res<CurrentTick>,
    buffer: Res<HistoryBufferTicks>,
    mut events: ResMut<TickedEvents<T>>,
) {
    let oldest = tick.0.saturating_sub(buffer.0);
    if oldest > 0 {
        events.prune_before(oldest);
    }
}

/// Type-erased truncation, so a rollback can rewind every registered event log
/// without knowing their types.
///
/// The alternative — leaving each log to fix itself when its tick is replayed —
/// looks like it works and does not. A replayed tick that produces an event
/// *replaces* what it said before, but a replayed tick that produces **nothing**
/// writes nothing, so there is no first-write to clear the stale entry with. The
/// prediction that never happened would be presented anyway.
#[derive(Resource, Default)]
pub struct TickedEventRegistry(Vec<fn(&mut World, u64)>);

impl TickedEventRegistry {
    /// Discard every registered log's events after `tick`, and rewind their
    /// presentation watermarks to match.
    pub fn truncate_all_after(world: &mut World, tick: u64) {
        let Some(registry) = world.get_resource::<Self>() else {
            return;
        };
        let truncators = registry.0.clone();
        for truncate in truncators {
            truncate(world, tick);
        }
    }

    /// Clear every registered log, for a session reset.
    pub fn clear_all(world: &mut World) {
        // Truncating before tick 0 empties the log and resets the watermark.
        let Some(registry) = world.get_resource::<Self>() else {
            return;
        };
        let truncators = registry.0.clone();
        for truncate in truncators {
            truncate(world, 0);
        }
    }
}

fn truncate_one<T: TickedEvent>(world: &mut World, tick: u64) {
    if let Some(mut events) = world.get_resource_mut::<TickedEvents<T>>() {
        events.truncate_after(tick);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Debug, PartialEq)]
    struct Thunk(u32);

    fn log() -> TickedEvents<Thunk> {
        TickedEvents::default()
    }

    #[test]
    fn a_replayed_tick_replaces_rather_than_appends() {
        // The property the whole design rests on. Without it, a client that
        // replays six ticks a frame reports six times as much as happened.
        let mut events = log();
        events.write(5, Thunk(1));
        events.write(5, Thunk(2));
        assert_eq!(events.at_tick(5), &[Thunk(1), Thunk(2)]);

        // Tick 5 runs again, as a rollback replay. Same two events.
        events.write(6, Thunk(9)); // moving on...
        events.write(5, Thunk(1)); // ...then back, which starts 5 over
        events.write(5, Thunk(2));
        assert_eq!(
            events.at_tick(5),
            &[Thunk(1), Thunk(2)],
            "a replay must not accumulate"
        );
    }

    #[test]
    fn each_event_is_presented_exactly_once() {
        let mut events = log();
        events.write(1, Thunk(1));
        events.write(2, Thunk(2));

        assert_eq!(
            events.drain_unpresented(2),
            vec![(1, Thunk(1)), (2, Thunk(2))]
        );
        assert!(
            events.drain_unpresented(2).is_empty(),
            "a second read must see nothing"
        );

        events.write(3, Thunk(3));
        assert_eq!(events.drain_unpresented(3), vec![(3, Thunk(3))]);
    }

    #[test]
    fn nothing_beyond_the_current_tick_is_presented() {
        // Ticks the simulation has run but the reader has not caught up to yet
        // must wait, or a rollback could unpresent something already shown.
        let mut events = log();
        events.write(1, Thunk(1));
        events.write(5, Thunk(5));
        assert_eq!(events.drain_unpresented(3), vec![(1, Thunk(1))]);
        assert_eq!(events.drain_unpresented(5), vec![(5, Thunk(5))]);
    }

    #[test]
    fn a_correction_unpresents_the_prediction_it_replaces() {
        // The half that is easy to forget and impossible to notice: truncation
        // must rewind the watermark, or the corrected version of a tick is
        // swallowed as "already shown" and the player hears the guess and never
        // the truth.
        let mut events = log();
        events.write(1, Thunk(1));
        events.write(2, Thunk(2));
        assert_eq!(events.drain_unpresented(2).len(), 2);

        events.truncate_after(1);
        events.write(2, Thunk(99));
        assert_eq!(
            events.drain_unpresented(2),
            vec![(2, Thunk(99))],
            "the corrected tick 2 must be presented, and tick 1 must not repeat"
        );
    }

    #[test]
    fn truncation_drops_the_ticks_that_never_happened() {
        let mut events = log();
        events.write(1, Thunk(1));
        events.write(2, Thunk(2));
        events.write(3, Thunk(3));
        events.truncate_after(1);
        assert_eq!(events.at_tick(2), &[] as &[Thunk]);
        assert_eq!(events.at_tick(3), &[] as &[Thunk]);
        assert_eq!(events.at_tick(1), &[Thunk(1)]);
    }

    #[test]
    fn pruning_does_not_resurrect_what_was_already_presented() {
        let mut events = log();
        events.write(1, Thunk(1));
        events.write(2, Thunk(2));
        assert_eq!(events.drain_unpresented(2).len(), 2);
        events.prune_before(2);
        assert!(
            events.drain_unpresented(2).is_empty(),
            "pruning must not move the watermark backwards"
        );
    }
}
