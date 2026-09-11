//! Input sampled on the tick: stamped for the tick it runs in, at no extra frame, once per
//! tick however many ticks a frame runs.

use std::time::Duration;

use bevy::prelude::*;
use bevy::time::TimeUpdateStrategy;
use bevy_ticked::prelude::*;
use bevy_ticked::tick::{CurrentTick, TickHoldReason, TickHolds};
use bevy_ticked_networking::input::InputQueue;
use bevy_ticked_networking::input_plugin::{LocalPlayer, TickedInputPlugin};
use serde::{Deserialize, Serialize};

const TICK: Duration = Duration::from_micros(15_625);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
struct Input(u32);

/// What the keyboard says this frame, stood in for by a resource the test sets.
#[derive(Resource, Default)]
struct Pressed(u32);

/// `(tick, input the simulation read at that tick)`.
#[derive(Resource, Default)]
struct Seen(Vec<(u64, Option<Input>)>);

#[derive(Resource, Default)]
struct Samples(u32);

fn sample(pressed: Res<Pressed>, mut samples: ResMut<Samples>) -> Input {
    samples.0 += 1;
    Input(pressed.0)
}

fn simulate(
    tick: Res<CurrentTick>,
    local: Res<LocalPlayer>,
    queue: Res<InputQueue<Input>>,
    mut seen: ResMut<Seen>,
) {
    seen.0.push((tick.0, queue.get(tick.0, local.0).copied()));
}

fn app_with(frame: Duration, build: impl FnOnce(&mut App)) -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .add_plugins(TickedPlugin {
            source: TickSource::Hz(64.0),
            ..default()
        })
        .insert_resource(TimeUpdateStrategy::ManualDuration(frame))
        .init_resource::<Pressed>()
        .init_resource::<Seen>()
        .init_resource::<Samples>()
        .add_systems(TickedSimulation, simulate);
    build(&mut app);
    app.finish();
    app.cleanup();
    app
}

#[test]
fn input_sampled_by_the_plugin_is_stamped_for_the_tick_it_will_run_in() {
    let mut app = app_with(TICK, |app| {
        app.add_plugins(TickedInputPlugin::<Input>::new(sample));
    });
    app.update();
    for frame in 1..=8u32 {
        app.world_mut().resource_mut::<Pressed>().0 = frame;
        app.update();
        let (tick, input) = *app.world().resource::<Seen>().0.last().unwrap();
        assert_eq!(
            input,
            Some(Input(frame)),
            "tick {tick}, run in the frame that pressed {frame}, read something else"
        );
    }
}

/// The old way: a capture system in `Update` stamps `tick + 1` after the tick has run, so a
/// press waits a whole frame before a tick reads it. The hook reads it in the same frame.
#[test]
fn sampling_in_the_hook_costs_no_extra_frame() {
    fn capture_in_update(
        pressed: Res<Pressed>,
        tick: Res<CurrentTick>,
        local: Res<LocalPlayer>,
        mut queue: ResMut<InputQueue<Input>>,
    ) {
        queue.insert(tick.0 + 1, local.0, Input(pressed.0));
    }
    let latency = |app: &mut App| {
        app.update();
        app.update();
        app.world_mut().resource_mut::<Pressed>().0 = 42;
        for frame in 0..4u64 {
            app.update();
            if app.world().resource::<Seen>().0.last().unwrap().1 == Some(Input(42)) {
                return frame;
            }
        }
        panic!("the press never reached a tick");
    };

    let mut old = app_with(TICK, |app| {
        app.init_resource::<LocalPlayer>()
            .init_resource::<InputQueue<Input>>()
            .add_systems(Update, capture_in_update);
    });
    let mut new = app_with(TICK, |app| {
        app.add_plugins(TickedInputPlugin::<Input>::new(sample));
    });
    let (old_frames, new_frames) = (latency(&mut old), latency(&mut new));
    println!(
        "frames from press to the tick that read it: Update capture {old_frames}, hook {new_frames}"
    );
    assert_eq!(
        new_frames, 0,
        "the hook's press is read by a tick in the same frame"
    );
    assert!(
        old_frames > new_frames,
        "the Update capture was not slower ({old_frames} vs {new_frames}), so the hook buys nothing"
    );
}

#[test]
fn a_frame_that_runs_two_ticks_samples_twice() {
    let mut app = app_with(TICK * 2, |app| {
        app.add_plugins(TickedInputPlugin::<Input>::new(sample));
    });
    app.update();
    let before = app.world().resource::<Samples>().0;
    let seen_before = app.world().resource::<Seen>().0.len();
    app.world_mut().resource_mut::<Pressed>().0 = 9;
    app.update();
    let samples = app.world().resource::<Samples>().0 - before;
    let seen = &app.world().resource::<Seen>().0[seen_before..];
    assert_eq!(seen.len(), 2, "the frame ran {} ticks", seen.len());
    assert_eq!(samples, 2, "two ticks, {samples} samples");
    assert!(
        seen.iter().all(|(_, input)| *input == Some(Input(9))),
        "both ticks read the press: {seen:?}"
    );
}

/// A scrub backwards runs the loop on a restore pass; the sampler stays out of it, so the
/// rewound tick keeps the input it ran with.
#[test]
fn a_restore_pass_does_not_resample() {
    let mut app = app_with(TICK, |app| {
        app.add_plugins(TickedInputPlugin::<Input>::new(sample));
    });
    for frame in 1..=8u32 {
        app.world_mut().resource_mut::<Pressed>().0 = frame;
        app.update();
    }
    let at_tick = app.world().resource::<CurrentTick>().0;
    let filed = app
        .world()
        .resource::<InputQueue<Input>>()
        .get(at_tick, 0)
        .copied();
    // A scrub: the clock held, one step back, a few frames of nothing.
    app.world_mut()
        .resource_mut::<TickHolds>()
        .hold(TickHoldReason::Manual);
    app.world_mut().resource_mut::<Pressed>().0 = 99;
    app.world_mut().write_message(StepBackward);
    app.update();
    app.update();
    assert_eq!(app.world().resource::<CurrentTick>().0, at_tick - 1);
    let after = app
        .world()
        .resource::<InputQueue<Input>>()
        .get(at_tick, 0)
        .copied();
    assert_eq!(after, filed, "the rewound tick's input was resampled");
}
