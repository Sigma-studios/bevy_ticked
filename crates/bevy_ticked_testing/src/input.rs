//! Queueing input for the tick about to run, and scripting it by frame.
//!
//! # The off-by-one every consumer fell into
//!
//! The simulation reads `InputQueue::at_tick(CurrentTick)` from *inside* the tick, after the
//! counter has been incremented. Between frames `CurrentTick` is the tick that has already run,
//! so input filed at `CurrentTick` lands where nothing will ever read it, and the test that filed
//! it then asserts that the body did not move — and passes, because it didn't. Three consumers
//! wrote `insert(tick, ...)`, watched nothing happen, and each added a `+ 1` with a comment that
//! said "not sure why". [`queue_input`] is the `+ 1`, with the reason.

use std::collections::BTreeMap;

use bevy::prelude::*;
use bevy_ensemble_loopback::PeerId;
use bevy_ticked::tick::CurrentTick;
use bevy_ticked_networking::input::{InputQueue, TickedInput};

use crate::net::TickedNetwork;

/// File `input` from `uuid` for the tick this peer is about to run — `CurrentTick + 1`, which is
/// the tick `apply_inputs` will read on the next frame. See the module docs for why not
/// `CurrentTick`.
pub fn queue_input<I: TickedInput>(app: &mut App, uuid: u128, input: I) {
    let tick = app.world().resource::<CurrentTick>().0;
    queue_input_at(app, tick + 1, uuid, input);
}

/// File `input` from `uuid` for an explicit tick. For a test that is asserting about the tick
/// arithmetic itself; everything else wants [`queue_input`].
pub fn queue_input_at<I: TickedInput>(app: &mut App, tick: u64, uuid: u128, input: I) {
    app.world_mut()
        .resource_mut::<InputQueue<I>>()
        .insert(tick, uuid, input);
}

/// Who presses what, on which frame — relative to the frame the script starts running on.
///
/// Frames rather than ticks, because a test controls frames: which tick a frame turns into on a
/// steering client is the stack's decision, and a script keyed by tick would have to know it.
#[derive(Clone, Debug)]
pub struct ActionScript<A: Clone> {
    frames: BTreeMap<u64, Vec<(u128, A)>>,
}

impl<A: Clone> Default for ActionScript<A> {
    fn default() -> Self {
        Self {
            frames: BTreeMap::new(),
        }
    }
}

impl<A: Clone> ActionScript<A> {
    pub fn new() -> Self {
        Self::default()
    }

    /// `player` does `action` on `frame`.
    pub fn at(mut self, frame: u64, player: u128, action: A) -> Self {
        self.frames.entry(frame).or_default().push((player, action));
        self
    }

    /// `player` does `action` on every frame from `from` to `to`, inclusive — a held key.
    pub fn hold(mut self, from: u64, to: u64, player: u128, action: A) -> Self {
        for frame in from..=to {
            self.frames
                .entry(frame)
                .or_default()
                .push((player, action.clone()));
        }
        self
    }

    /// Everything that happens on `frame`, in the order it was scripted.
    pub fn for_frame(&self, frame: u64) -> &[(u128, A)] {
        self.frames.get(&frame).map_or(&[], Vec::as_slice)
    }

    /// The last frame anything happens on.
    pub fn last_frame(&self) -> Option<u64> {
        self.frames.keys().next_back().copied()
    }

    /// How many frames have something scripted.
    pub fn len(&self) -> usize {
        self.frames.len()
    }

    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }
}

impl TickedNetwork {
    /// Play `script` from the next frame: each frame, queue that frame's actions on the peer
    /// owning each player, then step; then `trailing` more frames for the last of them to reach
    /// the host and come back.
    ///
    /// # Panics
    ///
    /// If a scripted player is not on the network.
    pub fn run_input_script<I: TickedInput>(&mut self, script: &ActionScript<I>, trailing: usize) {
        if let Some(last) = script.last_frame() {
            for frame in 0..=last {
                for (uuid, action) in script.for_frame(frame) {
                    let peer = self.peer_by_uuid(*uuid).unwrap_or_else(|| {
                        panic!("the script has player {uuid} pressing on frame {frame}, but no peer on this network owns that uuid")
                    });
                    queue_input(self.app_mut(peer), *uuid, action.clone());
                }
                self.step();
            }
        }
        self.run(trailing);
    }

    /// `peer` holds `input` for `frames` frames: queued for the tick about to run, then a step,
    /// `frames` times.
    pub fn hold_input<I: TickedInput>(&mut self, peer: PeerId, input: I, frames: usize) {
        let uuid = self.uuid(peer);
        for _ in 0..frames {
            queue_input(self.app_mut(peer), uuid, input.clone());
            self.step();
        }
    }
}
