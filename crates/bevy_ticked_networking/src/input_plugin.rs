//! Sampling the local player's input on the tick, not the frame.
//!
//! Every game wrote the same `capture_local_input` system in `Update`: read the keyboard,
//! build an input, `queue.insert(tick + 1, my_uuid, input)`. Three things were wrong with it
//! and every game had at least one. It ran once per frame, so a frame that ran two ticks fed
//! the second tick nothing (the queue held the last input, and `at_tick_or_last` hid the
//! hole). It ran after the tick, so a keypress waited a frame before the tick that read it. And
//! it needed the player's identity, which lived in three resources depending on the role.
//!
//! [`TickedInputPlugin`] runs the game's sampler inside [`TickedLoop`] in
//! [`TickedSystems::SampleInput`]: after the client's rollback, before the tick, once per tick.
//! The sampler is an ordinary system that returns the input (or `Option<Input>` for "nothing
//! this tick"); the plugin stamps it for the tick about to run under [`LocalPlayer`], which the
//! role plugins keep current. The client's `send_local_input` reads the same queue entry.
//!
//! The sampler is skipped on a restore pass (a scrub backwards through history) and while the
//! clock is held: a rewound tick keeps the input it ran with the first time, and a manual step
//! forward samples once for the tick it runs.

use std::marker::PhantomData;
use std::sync::Mutex;

use bevy::ecs::system::IntoSystem;
use bevy::prelude::*;
use bevy_ticked::tick::CurrentTick;
use bevy_ticked::tick::TickHolds;
use bevy_ticked::{RestoredThisPass, StepOnce, TickedLoop, TickedSystems};

use crate::input::{InputQueue, TickedInput};

/// The local player's uuid, whatever the role: the host's own, the client's, or `0` solo.
///
/// Set by `reset_on_host` and `reset_on_join`, cleared to `0` by `reset_on_leave`. A game
/// reads this rather than choosing between `LocalServerPlayer` and `LocalClientPlayer`.
#[derive(Resource, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LocalPlayer(pub u128);

/// What a sampler may return: the input, or `None` for "nothing to file this tick".
pub trait Sampled<I> {
    fn into_sampled(self) -> Option<I>;
}

impl<I: TickedInput> Sampled<I> for I {
    fn into_sampled(self) -> Option<I> {
        Some(self)
    }
}

impl<I: TickedInput> Sampled<I> for Option<I> {
    fn into_sampled(self) -> Option<I> {
        self
    }
}

/// Runs `sampler` once per tick and files what it returns for the tick about to run.
///
/// ```ignore
/// fn sample(keys: Res<ButtonInput<KeyCode>>) -> PlayerInput {
///     PlayerInput { jump: keys.pressed(KeyCode::Space), ..default() }
/// }
/// app.add_plugins(TickedInputPlugin::<PlayerInput>::new(sample));
/// ```
pub struct TickedInputPlugin<I> {
    install: Mutex<Option<Box<dyn FnOnce(&mut App) + Send + Sync>>>,
    _phantom: PhantomData<fn() -> I>,
}

impl<I: TickedInput> TickedInputPlugin<I> {
    pub fn new<S, O, M>(sampler: S) -> Self
    where
        S: IntoSystem<(), O, M> + Send + Sync + 'static,
        O: Sampled<I> + 'static,
        M: 'static,
    {
        let install = move |app: &mut App| {
            app.add_systems(
                TickedLoop,
                sampler
                    .pipe(file_sampled::<I, O>)
                    .run_if(not(resource_exists::<RestoredThisPass>))
                    .in_set(TickedSystems::SampleInput),
            );
        };
        Self {
            install: Mutex::new(Some(Box::new(install))),
            _phantom: PhantomData,
        }
    }
}

impl<I: TickedInput> Plugin for TickedInputPlugin<I> {
    fn build(&self, app: &mut App) {
        let install = self
            .install
            .lock()
            .expect("the sampler lock is never poisoned")
            .take()
            .expect("a TickedInputPlugin is built once");
        app.init_resource::<LocalPlayer>()
            .init_resource::<InputQueue<I>>();
        install(app);
    }
}

fn file_sampled<I: TickedInput, O: Sampled<I>>(
    In(sampled): In<O>,
    tick: Res<CurrentTick>,
    local: Res<LocalPlayer>,
    holds: Res<TickHolds>,
    step_once: Option<Res<StepOnce>>,
    mut queue: ResMut<InputQueue<I>>,
) {
    // A held clock runs no tick: filing for "the tick about to run" would overwrite the input
    // a scrubbed-back tick ran with. A manual step is a tick, and samples.
    if holds.is_held() && step_once.is_none() {
        return;
    }
    if let Some(input) = sampled.into_sampled() {
        queue.insert(tick.0 + 1, local.0, input);
    }
}
