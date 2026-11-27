//! What the host does with an input that arrives after its tick: how it is counted, what margin
//! it reports, and how a press in it survives.
//!
//! The margin reported is the worst first-arrival margin since the last report, so a client's lead
//! can cover its unlucky inputs rather than whichever one happened to be newest; and with a
//! [`LateInputFold`], a press overtaken by its own release on the way to the host is still
//! pressed, a tick late.

use std::time::Duration;

use bevy::prelude::*;
use bevy::time::TimeUpdateStrategy;
use bevy_ticked::prelude::*;
use bevy_ticked_networking::diagnostics::InputStats;
use bevy_ticked_networking::input::{InputQueue, LateInputFold};
use bevy_ticked_networking::messages::ReceivedNetworkInput;
use bevy_ticked_networking::prelude::*;
use bevy_ticked_networking::server::{InputMargins, LocalServerPlayer, MarginsReported};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
struct Input {
    walk: i8,
    jump: bool,
}

const HOST: u128 = 1;
const A: u128 = 7;
const TICK: Duration = Duration::from_micros(15_625);

/// A press is `jump` held in `late` and not in `ran`.
fn carry_jump(next: &mut Input, late: &Input, ran: &Input) {
    next.jump |= late.jump && !ran.jump;
}

const FOLD: LateInputFold<Input> = LateInputFold {
    fold: carry_jump,
    max_age: Ticks(6),
};

fn host(fold: bool) -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .add_plugins(TickedPlugin {
            source: TickSource::Hz(64.0),
            ..default()
        })
        .insert_resource(TimeUpdateStrategy::ManualDuration(TICK))
        .add_plugins(TickedServerPlugin::<Input>::new());
    app.insert_resource(LocalServerPlayer(HOST));
    if fold {
        app.insert_resource(FOLD);
    }
    app
}

fn current(app: &App) -> Tick {
    app.world().resource::<CurrentTick>().0
}

fn run_to_tick(app: &mut App, tick: Tick) {
    for _ in 0..(tick.0 as usize * 4 + 8) {
        if current(app) >= tick {
            return;
        }
        app.update();
    }
    panic!("the host never reached tick {tick}");
}

fn send(app: &mut App, tick: Tick, input: Input) {
    app.world_mut().trigger(ReceivedNetworkInput {
        sender: A,
        tick,
        input,
    });
}

fn margin(app: &App) -> i64 {
    app.world().resource::<InputMargins>().0[&A].ticks
}

fn next_input(app: &App) -> Input {
    let next = current(app).next();
    *app.world()
        .resource::<InputQueue<Input>>()
        .get(next, A)
        .expect("something is filed on the next tick")
}

const IDLE: Input = Input {
    walk: 0,
    jump: false,
};
const PRESS: Input = Input {
    walk: 0,
    jump: true,
};

// ── The margin ───────────────────────────────────────────────────────────────

#[test]
fn the_margin_is_the_worst_first_arrival_since_the_last_report() {
    let mut app = host(false);
    run_to_tick(&mut app, Tick(20));
    let now = current(&app);

    send(&mut app, now + Ticks(5), IDLE);
    send(&mut app, now + Ticks(1), IDLE);
    send(&mut app, now + Ticks(3), IDLE);
    assert_eq!(margin(&app), 1, "the worst of the three, not the latest");

    // A redundant copy of an input already here says nothing about how late inputs are.
    send(&mut app, now + Ticks(1), IDLE);
    send(&mut app, now - Ticks(2), IDLE);
    let before = margin(&app);
    send(&mut app, now - Ticks(2), IDLE);
    assert_eq!(
        margin(&app),
        before,
        "a second copy does not move the margin"
    );
}

#[test]
fn a_report_starts_a_new_worst() {
    let mut app = host(false);
    run_to_tick(&mut app, Tick(20));
    let now = current(&app);
    send(&mut app, now + Ticks(1), IDLE);
    // What a snapshot to this client does once it has carried the margin.
    app.world_mut()
        .resource_mut::<MarginsReported>()
        .0
        .insert(A);

    send(&mut app, now + Ticks(4), IDLE);
    assert_eq!(
        margin(&app),
        4,
        "the reported worst is not folded into the next one"
    );
    send(&mut app, now + Ticks(6), IDLE);
    assert_eq!(margin(&app), 4);
}

#[test]
fn each_tick_is_counted_once_however_many_copies_arrive() {
    let mut app = host(false);
    run_to_tick(&mut app, Tick(20));
    let now = current(&app);

    send(&mut app, now + Ticks(2), IDLE);
    send(&mut app, now + Ticks(2), IDLE);
    send(&mut app, now, IDLE);
    send(&mut app, now, IDLE);

    let stats = *app.world().resource::<InputStats>();
    assert_eq!(stats.received, 4);
    assert_eq!(stats.new_ticks, 2);
    assert_eq!(
        stats.late_new_ticks, 1,
        "the input for the tick the host has just run is late, once"
    );
    assert_eq!(stats.late, 2, "both copies of it were late");
}

// ── Late presses ─────────────────────────────────────────────────────────────

/// A host that has just run a tick with this player holding `input`, stamped for an earlier tick
/// than anything a test sends after it, so the test's inputs are the newest the host has had.
fn host_that_ran(fold: bool, input: Input) -> App {
    let mut app = host(fold);
    run_to_tick(&mut app, Tick(20));
    let now = current(&app);
    send(&mut app, now - Ticks(1), input);
    run_to_tick(&mut app, now.next());
    app
}

fn idle_host(fold: bool) -> App {
    host_that_ran(fold, IDLE)
}

#[test]
fn a_tap_overtaken_by_its_release_is_still_pressed() {
    let mut app = idle_host(true);
    let now = current(&app);
    // One packet, both late: the press, then the release that overtakes it on the next tick.
    send(&mut app, now - Ticks(1), PRESS);
    send(
        &mut app,
        now,
        Input {
            walk: 1,
            jump: false,
        },
    );
    let next = next_input(&app);
    assert!(next.jump, "the press is run on the next tick");
    assert_eq!(next.walk, 1, "and the newest input still says what is held");

    // The input meant for that tick arrives too, and the press survives it.
    send(
        &mut app,
        now.next(),
        Input {
            walk: 1,
            jump: false,
        },
    );
    assert!(next_input(&app).jump, "carried through the on-time input");
}

#[test]
fn without_a_fold_the_release_wins() {
    let mut app = idle_host(false);
    let now = current(&app);
    send(&mut app, now - Ticks(1), PRESS);
    send(&mut app, now, IDLE);
    assert!(
        !next_input(&app).jump,
        "a game that registers nothing keeps the old behaviour"
    );
}

#[test]
fn a_press_in_a_straggler_is_pressed() {
    let mut app = idle_host(true);
    let now = current(&app);
    // The release came first; the press, older, after it.
    send(&mut app, now, IDLE);
    send(&mut app, now - Ticks(1), PRESS);
    assert!(next_input(&app).jump);
}

#[test]
fn a_press_older_than_the_limit_is_not_carried() {
    let mut app = idle_host(true);
    let now = current(&app);
    send(&mut app, now, IDLE);
    send(&mut app, now - Ticks(7), PRESS);
    assert!(
        !next_input(&app).jump,
        "seven ticks is past a six-tick limit"
    );
}

#[test]
fn letting_go_of_a_held_button_stays_a_release() {
    let mut app = host_that_ran(true, PRESS);
    let now = current(&app);
    // Held all along, then let go: no press anywhere in that.
    send(&mut app, now - Ticks(1), PRESS);
    send(&mut app, now, IDLE);
    assert!(!next_input(&app).jump);
}
