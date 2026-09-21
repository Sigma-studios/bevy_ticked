//! A host that cannot keep up, which is the case the two edge-triggered pauses cannot see.
//!
//! `auto_pause_after_real_gap` asks "did a frame arrive long after the last one?" and the focus
//! rule asks "is the window focused?". Between them sits a host that is simply too slow: at three
//! frames a second its frame delta is 333 ms, under the 500 ms gap, and it needs 21 ticks a frame
//! against a `MaxTicksPerFrame` budget of 16. It discards the remainder every frame, its clock
//! falls behind real time for good, and every client keeps its own 64 Hz clock and piles up lead
//! that each snapshot then takes back — for as long as it lasts, with nothing said.
//!
//! Every frame here is 300 ms: deliberately longer than the 250 ms budget (16 ticks at 64 Hz) and
//! shorter than the 500 ms gap, so nothing but `auto_pause_when_behind_for` can fire.

use std::time::Duration;

use bevy::prelude::*;
use bevy::time::TimeUpdateStrategy;
use bevy_ticked::prelude::*;
use bevy_ticked_networking::diagnostics::HealthWarnings;
use bevy_ticked_networking::pause::HostBehind;
use bevy_ticked_networking::prelude::*;
use bevy_ticked_networking::server::LocalServerPlayer;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
struct Input;

const LOCAL: u128 = 7;
/// One tick at 64 Hz.
const TICK: Duration = Duration::from_micros(15_625);
/// Longer than the 250 ms budget, shorter than the 500 ms gap.
const TOO_LONG: Duration = Duration::from_millis(300);

fn host() -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .add_plugins(TickedPlugin {
            source: TickSource::Hz(64.0),
            ..default()
        })
        .add_plugins(TickedServerPlugin::<Input>::new())
        // A host-only app has no diagnostics of its own, which is why the guard asks for this
        // one optionally. The assertions below want to read the count, so this harness has it.
        .init_resource::<HealthWarnings>()
        .insert_resource(TimeUpdateStrategy::ManualDuration(TICK));
    app.insert_resource(LocalServerPlayer(LOCAL));
    app.update();
    app
}

fn frames(app: &mut App, each: Duration, count: usize) {
    app.insert_resource(TimeUpdateStrategy::ManualDuration(each));
    for _ in 0..count {
        app.update();
    }
}

fn paused(app: &App) -> Option<PauseReason> {
    app.world().resource::<SessionPause>().0.map(|p| p.reason)
}

#[test]
fn a_host_that_cannot_keep_up_pauses_and_resumes_when_it_can_again() {
    let mut app = host();
    assert_eq!(paused(&app), None, "nothing is wrong yet");

    // Two bad frames are a hitch, not a verdict: the default asks for three.
    frames(&mut app, TOO_LONG, 2);
    assert_eq!(
        paused(&app),
        None,
        "two frames over budget must not pause a session — every game has a heavy frame"
    );
    assert_eq!(
        app.world().resource::<HostBehind>().consecutive_frames,
        2,
        "but they are counted"
    );

    frames(&mut app, TOO_LONG, 1);
    assert_eq!(
        paused(&app),
        Some(PauseReason::HostTooSlow),
        "the third says the host cannot keep up. Not `HostStalled`: 300 ms is under the 500 ms \
         gap, so the edge-triggered rule never saw this at all"
    );
    assert_eq!(
        app.world()
            .resource::<HealthWarnings>()
            .host_behind_real_time,
        1,
        "and it is counted where a session's health is read, not only shown as a curtain"
    );

    // Still slow, and still paused: the pause holds while the frames stay long. This is why the
    // guard measures frame time rather than the backlogs the loop discards — held, it discards
    // none, so a discard-counting version would resume here and pause again next frame, for ever.
    frames(&mut app, TOO_LONG, 4);
    assert_eq!(
        paused(&app),
        Some(PauseReason::HostTooSlow),
        "a paused host is not a host that has recovered"
    );

    // The frames come back.
    frames(&mut app, TICK, 2);
    assert_eq!(
        paused(&app),
        None,
        "and the session resumes on its own, without the game asking"
    );
    assert_eq!(
        app.world().resource::<HostBehind>().consecutive_frames,
        0,
        "with nothing left owing"
    );
}

#[test]
fn a_host_keeping_up_is_never_paused_however_long_it_runs() {
    // The other half, and the reason the threshold is the budget and not a frame rate: a host at
    // ten frames a second needs 6.4 ticks a frame against a budget of 16. It is *fine* — every
    // tick runs, the clock keeps real-time pace — and pausing it would be wrong.
    let mut app = host();
    frames(&mut app, Duration::from_millis(100), 64);
    assert_eq!(
        paused(&app),
        None,
        "10 FPS is within the catch-up budget: the host runs all 64 ticks a second, in bursts"
    );
    assert_eq!(
        app.world().resource::<HostBehind>().frames,
        0,
        "and not one frame was ever behind"
    );
}

#[test]
fn the_guard_can_be_turned_off() {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .add_plugins(TickedPlugin {
            source: TickSource::Hz(64.0),
            ..default()
        })
        // Before the role plugin, which is the documented way to keep a policy of one's own.
        .insert_resource(PausePolicy {
            auto_pause_when_behind_for: None,
            ..default()
        })
        .add_plugins(TickedServerPlugin::<Input>::new())
        .insert_resource(TimeUpdateStrategy::ManualDuration(TICK));
    app.insert_resource(LocalServerPlayer(LOCAL));
    app.update();

    frames(&mut app, TOO_LONG, 8);
    assert_eq!(
        paused(&app),
        None,
        "a game that would rather run badly than stop is allowed to"
    );
}
