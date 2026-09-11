//! Noticing that two peers' worlds have stopped agreeing, and when.
//!
//! Lockstep's entire premise is that identical actions applied to identical state produce
//! identical state, and a rollback client's is that its replay of the authority's ticks lands on
//! the authority's state. Nothing used to check whether either held. "Desync" was something people
//! reported by describing what they saw, and by the time it was visible — a building on one
//! screen and not the other — it had usually happened hundreds of ticks earlier.
//!
//! The split this module draws is the one worth drawing:
//!
//! * **The game owns what goes into the hash.** Which components are simulation state and which
//!   are local view, whether a rendering transform counts, what a "section" of the world is.
//!   That is a statement about a particular simulation, and no crate can make it for you. It is
//!   the [`WorldHash`] impl.
//! * **This module owns the log and the search.** Sampling every N ticks, keeping a bounded
//!   history, and — the part that actually saves the day — reporting the *first* tick two peers
//!   differed on rather than the first one somebody noticed. None of that mentions any
//!   particular game, and every networked game needs exactly it — which is why it lives in the
//!   core crate rather than in the lockstep one it was written for.
//!
//! # Using it
//!
//! ```ignore
//! #[derive(Clone, Copy, PartialEq, Debug)]
//! struct MyWorldHash { buildings: u64, players: u64, tick: u64 }
//!
//! impl WorldHash for MyWorldHash {
//!     fn sample(world: &mut World) -> Self { /* hash whatever must agree */ }
//!     fn value(&self) -> u64 { /* fold it to one number */ }
//!     fn differences(&self, other: &Self) -> Vec<&'static str> {
//!         // optional, and the difference between "they differ" and "the players differ"
//!     }
//! }
//!
//! app.add_plugins(ChecksumLogPlugin::<MyWorldHash>::default());
//! ```
//!
//! Then `ChecksumLog::<MyWorldHash>::first_divergence` on two peers' logs names the tick.
//!
//! # Sampling costs a walk of the world
//!
//! Hence [`ChecksumLog::interval`]. Every tick is affordable in a test and wasteful in play; the
//! default of 64 is once a second at 64 Hz. Two logs can only be compared at ticks they *both*
//! sampled, so peers that disagree about the interval will find nothing to compare.

use bevy::ecs::intern::Interned;
use bevy::prelude::*;
use crate::tick::CurrentTick;
use std::marker::PhantomData;

/// A game's reduction of its world to something two peers can compare.
///
/// Implement this on a plain `Copy` value — one tick's worth of hash, not a handle to the world.
pub trait WorldHash: Copy + PartialEq + Send + Sync + 'static {
    /// Hash the world as it stands.
    ///
    /// Called from inside the tick, so what it sees is the simulation's own state rather than
    /// anything a later frame has interpolated or drawn.
    fn sample(world: &mut World) -> Self;

    /// Fold the hash down to the single number that gets compared, logged and printed.
    fn value(&self) -> u64;

    /// Which named parts of the world differ from `other`, for a divergence report.
    ///
    /// Optional — the default says nothing, and a divergence then reports only the tick. Worth
    /// implementing: "the players differ but the buildings do not" and the reverse send you to
    /// opposite ends of a codebase, and working that out by bisecting a `u64` is miserable.
    fn differences(&self, other: &Self) -> Vec<&'static str> {
        let _ = other;
        Vec::new()
    }
}

/// A bounded history of this peer's own hashes, so a divergence can be dated.
///
/// Insert one yourself before adding [`ChecksumLogPlugin`] to change the interval or capacity.
#[derive(Resource, Debug)]
pub struct ChecksumLog<H: WorldHash> {
    /// How many ticks between samples. Every tick is affordable in a test and wasteful in play.
    pub interval: u64,
    /// `(tick, hash)`, oldest first, capped at `capacity`.
    pub samples: Vec<(u64, H)>,
    /// How many samples to keep before dropping the oldest.
    pub capacity: usize,
}

impl<H: WorldHash> Default for ChecksumLog<H> {
    fn default() -> Self {
        Self {
            interval: 64,
            samples: Vec::new(),
            capacity: 256,
        }
    }
}

impl<H: WorldHash> ChecksumLog<H> {
    /// One sample per tick, for tests that want the exact tick of a divergence.
    pub fn every_tick() -> Self {
        Self {
            interval: 1,
            ..Default::default()
        }
    }

    /// One sample every `interval` ticks.
    pub fn every(interval: u64) -> Self {
        Self {
            interval,
            ..Default::default()
        }
    }

    pub fn at(&self, tick: u64) -> Option<H> {
        self.samples
            .iter()
            .find(|(sample_tick, _)| *sample_tick == tick)
            .map(|(_, hash)| *hash)
    }

    pub fn latest(&self) -> Option<(u64, H)> {
        self.samples.last().copied()
    }

    /// The oldest sample still held, once [`capacity`](Self::capacity) has started dropping them.
    ///
    /// The mirror of [`latest`](Self::latest), and it answers a question a comparison against
    /// another peer has to ask: a tick *below* this one was sampled and then forgotten, which is
    /// a different thing from a tick that was never sampled at all. Reading the first as the
    /// second turns ordinary housekeeping into a warning about a mismatched interval.
    pub fn oldest(&self) -> Option<(u64, H)> {
        self.samples.first().copied()
    }

    /// The earliest tick both logs sampled where they disagree.
    ///
    /// This is the question worth asking after a desync: not "do we differ now", which is obvious
    /// by the time anyone looks, but "when did we start" — and, from the sections, "about what".
    pub fn first_divergence(&self, other: &ChecksumLog<H>) -> Option<Divergence<H>> {
        self.samples
            .iter()
            .filter_map(|(tick, mine)| {
                other
                    .at(*tick)
                    .filter(|theirs| theirs != mine)
                    .map(|theirs| Divergence {
                        tick: *tick,
                        left: *mine,
                        right: theirs,
                        sections: mine.differences(&theirs),
                    })
            })
            .min_by_key(|divergence| divergence.tick)
    }

    fn record(&mut self, tick: u64, hash: H) {
        self.samples.push((tick, hash));
        if self.samples.len() > self.capacity {
            self.samples.remove(0);
        }
    }
}

/// Two peers disagreeing about one tick, and what about.
#[derive(Clone, Debug, PartialEq)]
pub struct Divergence<H: WorldHash> {
    pub tick: u64,
    pub left: H,
    pub right: H,
    /// Which sections of the world differ, per [`WorldHash::differences`].
    pub sections: Vec<&'static str>,
}

impl<H: WorldHash> std::fmt::Display for Divergence<H> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "tick {} ({:#018x} vs {:#018x}), differing in: {}",
            self.tick,
            self.left.value(),
            self.right.value(),
            if self.sections.is_empty() {
                "nothing identifiable".to_string()
            } else {
                self.sections.join(", ")
            }
        )
    }
}

/// Records a [`WorldHash`] into [`ChecksumLog`] every `interval` ticks.
///
/// By default the sample runs in `TickedSimulation` with no ordering of its own, which is right
/// for a game whose whole tick is one set of systems. A game with tick phases wants it in the
/// last one — otherwise the hash describes a half-simulated tick — so pass that set to
/// [`in_set`](Self::in_set).
pub struct ChecksumLogPlugin<H: WorldHash> {
    set: Option<Interned<dyn SystemSet>>,
    marker: PhantomData<fn() -> H>,
}

impl<H: WorldHash> Default for ChecksumLogPlugin<H> {
    fn default() -> Self {
        Self {
            set: None,
            marker: PhantomData,
        }
    }
}

impl<H: WorldHash> ChecksumLogPlugin<H> {
    /// Sample inside `set` — the game's last tick phase, so the hash describes a finished tick.
    pub fn in_set(mut self, set: impl SystemSet) -> Self {
        self.set = Some(set.intern());
        self
    }
}

impl<H: WorldHash> Plugin for ChecksumLogPlugin<H> {
    fn build(&self, app: &mut App) {
        app.init_resource::<ChecksumLog<H>>();
        match self.set {
            Some(set) => {
                app.add_systems(
                    crate::TickedSimulation,
                    record_checksum::<H>.in_set(set),
                );
            }
            None => {
                app.add_systems(crate::TickedSimulation, record_checksum::<H>);
            }
        }
    }
}

/// The sampling system, public so a game can place it itself instead of using the plugin.
pub fn record_checksum<H: WorldHash>(world: &mut World) {
    let interval = world.resource::<ChecksumLog<H>>().interval;
    let tick = world.resource::<CurrentTick>().0;
    if interval == 0 || !tick.is_multiple_of(interval) {
        return;
    }

    let hash = H::sample(world);
    world.resource_mut::<ChecksumLog<H>>().record(tick, hash);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Copy, PartialEq, Debug)]
    struct TestHash {
        buildings: u64,
        players: u64,
    }

    impl WorldHash for TestHash {
        fn sample(_world: &mut World) -> Self {
            TestHash {
                buildings: 0,
                players: 0,
            }
        }

        fn value(&self) -> u64 {
            self.buildings ^ self.players
        }

        fn differences(&self, other: &Self) -> Vec<&'static str> {
            let mut differences = Vec::new();
            if self.buildings != other.buildings {
                differences.push("buildings");
            }
            if self.players != other.players {
                differences.push("players");
            }
            differences
        }
    }

    fn log_of(samples: &[(u64, u64, u64)]) -> ChecksumLog<TestHash> {
        let mut log = ChecksumLog::every_tick();
        for (tick, buildings, players) in samples {
            log.record(
                *tick,
                TestHash {
                    buildings: *buildings,
                    players: *players,
                },
            );
        }
        log
    }

    #[test]
    fn the_first_differing_tick_is_reported_not_the_latest() {
        let left = log_of(&[(1, 10, 20), (2, 11, 20), (3, 12, 20)]);
        let right = log_of(&[(1, 10, 20), (2, 99, 20), (3, 98, 20)]);

        let divergence = left
            .first_divergence(&right)
            .expect("these logs disagree on two ticks");

        assert_eq!(
            divergence.tick, 2,
            "reporting the newest difference points at a symptom hundreds of ticks after the \
             cause, which is the entire reason this search exists"
        );
        assert_eq!(divergence.sections, vec!["buildings"]);
    }

    #[test]
    fn agreement_is_not_a_divergence() {
        let left = log_of(&[(1, 10, 20), (2, 11, 21)]);
        let right = log_of(&[(1, 10, 20), (2, 11, 21)]);

        assert_eq!(left.first_divergence(&right), None);
    }

    #[test]
    fn only_ticks_both_peers_sampled_are_compared() {
        let left = log_of(&[(1, 10, 20), (2, 11, 20)]);
        let right = log_of(&[(2, 11, 20)]);

        assert_eq!(
            left.first_divergence(&right),
            None,
            "tick 1 exists on one side only, and an unsampled tick is not a disagreement"
        );
    }

    #[test]
    fn the_log_stays_bounded_and_keeps_the_newest() {
        let mut log = ChecksumLog::<TestHash>::every_tick();
        log.capacity = 3;
        for tick in 0..10 {
            log.record(
                tick,
                TestHash {
                    buildings: tick,
                    players: 0,
                },
            );
        }

        assert_eq!(log.samples.len(), 3);
        assert_eq!(log.latest().map(|(tick, _)| tick), Some(9));
        assert_eq!(log.at(0), None, "the oldest samples are the ones dropped");
    }
}
