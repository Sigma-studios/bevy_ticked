//! The host's input window, and the order the queue hands inputs out in.
//!
//! Two things a client could do to a host before this: stamp an input with any tick it liked,
//! and thereby put an entry in the queue that no prune would ever reach and a margin in every
//! snapshot that no lead could ever match; and — without meaning to — make the host fold over
//! this tick's inputs in a different order than every other peer, because the inner map was a
//! `HashMap` seeded per process.

use std::time::Duration;

use bevy::prelude::*;
use bevy::time::TimeUpdateStrategy;
use bevy_ticked::prelude::*;
use bevy_ticked_networking::diagnostics::InputStats;
use bevy_ticked_networking::input::{InputQueue, MAX_INPUT_LEAD_TICKS};
use bevy_ticked_networking::messages::{PeerLeft, ReceivedNetworkInput};
use bevy_ticked_networking::prelude::*;
use bevy_ticked_networking::server::{InputMargins, LocalServerPlayer, NewestInputTick};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
struct Input {
    forward: bool,
}

#[derive(Component, Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
struct Pos(i32);

const HOST: u128 = 1;
const A: u128 = 7;
const B: u128 = 9;
const TICK: Duration = Duration::from_micros(15_625);

fn host() -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .add_plugins(TickedPlugin {
            source: TickSource::Hz(64.0),
            ..default()
        })
        .insert_resource(TimeUpdateStrategy::ManualDuration(TICK))
        .add_plugins(TickedServerPlugin::<Input>::new())
        .register_networked_ticked_component::<Pos>("Pos");
    app.insert_resource(LocalServerPlayer(HOST));
    app
}

fn current(app: &App) -> u64 {
    app.world().resource::<CurrentTick>().0
}

/// Run frames until the host has simulated at least `tick`.
fn run_to_tick(app: &mut App, tick: u64) {
    for _ in 0..(tick as usize * 4 + 8) {
        if current(app) >= tick {
            return;
        }
        app.update();
    }
    panic!(
        "the host never reached tick {tick}; it is at {}",
        current(app)
    );
}

fn send(app: &mut App, sender: u128, tick: u64) {
    app.world_mut().trigger(ReceivedNetworkInput {
        sender,
        tick,
        input: Input { forward: true },
    });
}

fn stats(app: &App) -> InputStats {
    *app.world().resource::<InputStats>()
}

fn margin(app: &App, uuid: u128) -> Option<i64> {
    app.world().resource::<InputMargins>().0.get(&uuid).copied()
}

fn newest(app: &App, uuid: u128) -> Option<u64> {
    app.world()
        .resource::<NewestInputTick>()
        .0
        .get(&uuid)
        .copied()
}

fn queue(app: &App) -> &InputQueue<Input> {
    app.world().resource::<InputQueue<Input>>()
}

// ── The window ───────────────────────────────────────────────────────────────

#[test]
fn an_input_stamped_far_in_the_future_is_dropped() {
    let mut app = host();
    run_to_tick(&mut app, 8);
    let now = current(&app);

    send(&mut app, A, now + MAX_INPUT_LEAD_TICKS + 1);
    assert_eq!(stats(&app).dropped_out_of_window, 1);
    assert_eq!(
        stats(&app).received,
        0,
        "a dropped input is not a received one"
    );
    assert!(
        queue(&app).get(now + MAX_INPUT_LEAD_TICKS + 1, A).is_none(),
        "one tick past the client's lead ceiling is one tick no client can be at"
    );

    send(&mut app, A, now + MAX_INPUT_LEAD_TICKS);
    assert_eq!(stats(&app).dropped_out_of_window, 1);
    assert!(
        queue(&app).get(now + MAX_INPUT_LEAD_TICKS, A).is_some(),
        "the ceiling itself is a lead a client can legitimately hold"
    );
}

#[test]
fn an_input_older_than_the_window_is_dropped() {
    let mut app = host();
    app.insert_resource(HistoryBufferTicks(4));
    run_to_tick(&mut app, 10);
    let now = current(&app);

    send(&mut app, A, now - 5);
    assert_eq!(stats(&app).dropped_out_of_window, 1);
    assert!(queue(&app).get(now - 5, A).is_none());
    assert!(
        queue(&app).get(now + 1, A).is_none(),
        "a dropped input is not forward-filled either: it never entered"
    );
    assert_eq!(
        newest(&app, A),
        None,
        "and it did not become the newest thing heard from that sender"
    );

    send(&mut app, A, now - 4);
    assert_eq!(stats(&app).dropped_out_of_window, 1);
    assert!(
        queue(&app).get(now - 4, A).is_some(),
        "the oldest tick still in history is still replayable, so still accepted"
    );
    assert!(
        queue(&app).get(now + 1, A).is_some(),
        "and late-but-in-window still gets the next tick, as before"
    );
}

/// A minimal LCG: enough to spread ten thousand ticks over half of `u64`, and no dependency.
fn random_ticks(seed: u64, count: usize) -> impl Iterator<Item = u64> {
    let mut state = seed;
    std::iter::repeat_with(move || {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (state >> 1) % (u64::MAX / 2)
    })
    .take(count)
}

#[test]
fn the_input_queue_cannot_grow_under_hostile_input() {
    let mut app = host();
    run_to_tick(&mut app, 8);
    let now = current(&app);
    let window = app.world().resource::<HistoryBufferTicks>().0;
    let oldest = now.saturating_sub(window);
    let newest_allowed = now + MAX_INPUT_LEAD_TICKS;

    let mut expected_drops = 0;
    for tick in random_ticks(0xC0FFEE, 10_000) {
        if tick < oldest || tick > newest_allowed {
            expected_drops += 1;
        }
        send(&mut app, A, tick);
    }

    let bound = (window + MAX_INPUT_LEAD_TICKS + 2) as usize;
    assert!(
        queue(&app).inputs.len() <= bound,
        "the queue holds {} ticks; the window allows at most {bound}",
        queue(&app).inputs.len()
    );
    assert_eq!(stats(&app).dropped_out_of_window, expected_drops);
    assert_eq!(
        stats(&app).received + stats(&app).dropped_out_of_window,
        10_000,
        "every input was either accepted or counted as dropped"
    );
    for tick in queue(&app).inputs.keys() {
        assert!(
            (oldest..=newest_allowed).contains(tick),
            "tick {tick} is in the queue and outside [{oldest}, {newest_allowed}]"
        );
    }
}

#[test]
fn a_dropped_input_does_not_move_the_margin() {
    let mut app = host();
    app.insert_resource(HistoryBufferTicks(4));
    run_to_tick(&mut app, 10);
    let now = current(&app);

    send(&mut app, A, now + 2);
    assert_eq!(margin(&app, A), Some(2));
    assert_eq!(newest(&app, A), Some(now + 2));

    send(&mut app, A, now + 1_000);
    assert_eq!(
        margin(&app, A),
        Some(2),
        "a margin of a thousand would have every snapshot telling the client to shed a lead it \
         does not have"
    );
    assert_eq!(newest(&app, A), Some(now + 2));

    send(&mut app, A, now - 6);
    assert_eq!(margin(&app, A), Some(2));
    assert_eq!(newest(&app, A), Some(now + 2));
    assert_eq!(stats(&app).dropped_out_of_window, 2);
    assert_eq!(
        stats(&app).late,
        0,
        "a dropped input is not a late one either"
    );
}

// ── Leaving ──────────────────────────────────────────────────────────────────

#[test]
fn a_departed_players_inputs_and_margin_are_forgotten() {
    let mut app = host();
    run_to_tick(&mut app, 8);
    let now = current(&app);

    send(&mut app, A, now + 1);
    send(&mut app, A, now + 2);
    send(&mut app, B, now + 1);
    assert_eq!(queue(&app).players().collect::<Vec<_>>(), vec![A, B]);

    app.world_mut().trigger(PeerLeft(A));

    assert_eq!(
        queue(&app).players().collect::<Vec<_>>(),
        vec![B],
        "the departed player's inputs are gone from every tick"
    );
    assert!(queue(&app).get(now + 1, A).is_none());
    assert!(queue(&app).get(now + 2, A).is_none());
    assert!(
        queue(&app).get(now + 1, B).is_some(),
        "and the other player's are untouched"
    );
    assert_eq!(
        margin(&app, A),
        None,
        "no margin for a player who is not there"
    );
    assert_eq!(margin(&app, B), Some(1));
    assert_eq!(
        newest(&app, A),
        None,
        "a rejoin under the same uuid must not find its first inputs older than 'newest'"
    );
    assert_eq!(newest(&app, B), Some(now + 1));
}

// ── Order ────────────────────────────────────────────────────────────────────

#[test]
fn input_queue_at_tick_iterates_in_a_fixed_order() {
    let mut queue = InputQueue::<Input>::default();
    let shuffled = [9u128, 3, 7, 1, 8, 2, 6, 4, 5];
    for (i, uuid) in shuffled.into_iter().enumerate() {
        queue.insert(
            5,
            uuid,
            Input {
                forward: i % 2 == 0,
            },
        );
    }
    queue.insert(6, 3, Input::default());
    queue.insert(4, 11, Input::default());

    let order: Vec<u128> = queue.at_tick(5).unwrap().keys().copied().collect();
    assert_eq!(
        order,
        vec![1, 2, 3, 4, 5, 6, 7, 8, 9],
        "a simulation that folds over every player's input must see the same order on every peer"
    );
    assert_eq!(
        queue.players().collect::<Vec<_>>(),
        vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 11],
        "every uuid once, ascending, whatever tick it was seen at"
    );
}
