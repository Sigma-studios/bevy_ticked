//! Sampling the world hash on a lockstep peer: the core log, gated on there being a session.
//!
//! [`bevy_ticked::checksum::ChecksumLogPlugin`] samples on every tick that is a multiple of the
//! interval, and it cannot know whether there is anybody to compare against — the core crate
//! has no notion of a lobby. On a lockstep peer that meant the log filled while the player sat
//! in the menu, or played alone, with hashes of a world nobody else had. They were harmless
//! until the moment they were not: a player who idled a while and then joined had samples at
//! tick numbers the host was also at, of a different world, and the exchange compared them and
//! reported a desync at the first tick both logs happened to share.
//!
//! The exchange now clears the log when a join snapshot replaces the world, which is the fix
//! for that. This plugin is the other half: not taking the samples in the first place, because
//! a walk of the world once a second for a comparison that cannot happen is a cost for nothing.
//! A test that wants a solo peer's hashes — to compare two solo runs of the same script — says
//! so with [`sample_without_lobby`](ChecksumLogPlugin::sample_without_lobby).

use bevy::ecs::intern::Interned;
use bevy::prelude::*;
use bevy_ensemble::Lobby;
use std::marker::PhantomData;

use crate::checksum::{ChecksumLog, WorldHash, record_checksum};

/// Records a [`WorldHash`] into [`ChecksumLog`] every `interval` ticks, while there is a lobby.
///
/// The lockstep counterpart of [`bevy_ticked::checksum::ChecksumLogPlugin`], with the same
/// [`in_set`](Self::in_set): a game with tick phases samples in the last one, or the hash
/// describes a half-simulated tick. Add it alongside
/// [`ChecksumExchangePlugin`](crate::ChecksumExchangePlugin), which is what puts the samples on
/// the wire and compares what comes back.
pub struct ChecksumLogPlugin<H: WorldHash> {
    set: Option<Interned<dyn SystemSet>>,
    sample_without_lobby: bool,
    marker: PhantomData<fn() -> H>,
}

impl<H: WorldHash> Default for ChecksumLogPlugin<H> {
    fn default() -> Self {
        Self {
            set: None,
            sample_without_lobby: false,
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

    /// Sample even when this peer is in no lobby.
    ///
    /// Off by default: a solo world has nobody to disagree with, and the samples cost a walk
    /// of the world each. On for a test that compares two solo runs, or a game that keeps its
    /// own record of a single-player session.
    pub fn sample_without_lobby(mut self) -> Self {
        self.sample_without_lobby = true;
        self
    }
}

impl<H: WorldHash> Plugin for ChecksumLogPlugin<H> {
    fn build(&self, app: &mut App) {
        app.init_resource::<ChecksumLog<H>>();
        let always = self.sample_without_lobby;
        let in_a_lobby = move |lobbies: Query<(), With<Lobby>>| always || !lobbies.is_empty();
        let sample = record_checksum::<H>.run_if(in_a_lobby);
        match self.set {
            Some(set) => {
                app.add_systems(bevy_ticked::TickedSimulation, sample.in_set(set));
            }
            None => {
                app.add_systems(bevy_ticked::TickedSimulation, sample);
            }
        }
    }
}
